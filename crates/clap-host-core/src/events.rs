//! CLAP event plumbing: the lock-free queues feeding the audio thread, the
//! input event list, and raw-MIDI → CLAP dialect conversion.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use clap_sys::{
    events::{
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_CHOKE,
        CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
        CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE,
        clap_event_header, clap_event_midi, clap_event_note, clap_event_param_gesture,
        clap_event_param_value, clap_input_events, clap_output_events,
    },
    id::clap_id,
};

/// Raw 3-byte MIDI message, as delivered by midir.
pub type RawMidi = [u8; 3];

/// Queue depth. 1024 short messages is far more than one audio block sees.
pub const QUEUE_CAP: usize = 1024;

/// Anything the main/UI thread sends to the audio thread mid-stream.
#[derive(Copy, Clone, Debug)]
pub enum UiEvent {
    Param { id: clap_id, value: f64 },
    Midi(RawMidi),
}

/// Plugin → host events we care about (param feedback). Copied out of
/// `clap_process.out_events` / `params.flush` on the producing thread.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PluginOutEvent {
    Value { id: clap_id, value: f64 },
    GestureBegin { id: clap_id },
    GestureEnd { id: clap_id },
}

/// Note/MIDI events captured from `clap_process.out_events` on the timed path.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PluginNoteOut {
    Note {
        type_: u16,
        time: u32,
        flags: u32,
        note_id: i32,
        port_index: i16,
        channel: i16,
        key: i16,
        velocity: f64,
    },
    Midi {
        time: u32,
        flags: u32,
        port_index: u16,
        data: [u8; 3],
    },
}

/// Capacity of the preallocated timed note/MIDI output buffer.
pub const MAX_NOTE_OUT: usize = 256;

/// Preallocated plugin→host note/MIDI capture. Does not grow past `cap`.
pub struct NoteOutList {
    items: Vec<PluginNoteOut>,
    cap: usize,
    params: *const ArrayQueue<PluginOutEvent>,
}

impl NoteOutList {
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: Vec::with_capacity(cap),
            cap,
            params: ptr::null(),
        }
    }

    pub fn set_params(&mut self, params: &Queue<PluginOutEvent>) {
        self.params = Arc::as_ptr(params);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[must_use]
    pub fn as_slice(&self) -> &[PluginNoteOut] {
        &self.items
    }

    fn try_push_note(&mut self, ev: PluginNoteOut) -> bool {
        if self.items.len() >= self.cap {
            return false;
        }
        self.items.push(ev);
        true
    }
}

/// Lock-free MPMC queue. Shared by `Arc` rather than split into producer and
/// consumer halves: the MIDI thread, the UI thread and a rebuilt audio Engine
/// all need to hold on to the same queue across device switches.
pub type Queue<T> = Arc<ArrayQueue<T>>;

#[must_use]
pub fn queue<T>() -> Queue<T> {
    Arc::new(ArrayQueue::new(QUEUE_CAP))
}

/// Which note dialect the plugin's first input note port wants.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// Raw `CLAP_EVENT_MIDI`.
    Midi,
    /// `CLAP_EVENT_NOTE_ON` / `CLAP_EVENT_NOTE_OFF`.
    Clap,
    /// Plugin has no input note port — drop MIDI.
    None,
}

/// Storage large enough for any event kind we push. `#[repr(C)]` so the
/// leading `clap_event_header` is at offset 0 for every variant.
#[repr(C)]
#[derive(Copy, Clone)]
pub union EvStorage {
    hdr: clap_event_header,
    note: clap_event_note,
    midi: clap_event_midi,
    param: clap_event_param_value,
}

/// Input event list handed to `clap_process`. Rebuilt (cheaply) per block.
pub struct EvList {
    items: Vec<EvStorage>,
}

impl EvList {
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: Vec::with_capacity(cap),
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// `cookie` is the plugin's own `clap_param_info.cookie` for this param, if
    /// the host knows it — echoing it back lets the plugin skip its id lookup.
    pub fn push_param(&mut self, param_id: clap_id, value: f64, cookie: *mut c_void, time: u32) {
        self.items.push(EvStorage {
            param: clap_event_param_value {
                header: header(
                    CLAP_EVENT_PARAM_VALUE,
                    size_of::<clap_event_param_value>(),
                    time,
                ),
                param_id,
                cookie,
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value,
            },
        });
    }

    /// Translate one raw MIDI message into the plugin's preferred dialect.
    /// Messages the dialect cannot express are dropped.
    pub fn push_midi(&mut self, msg: RawMidi, dialect: Dialect, time: u32) {
        self.push_midi_with_flags(msg, dialect, time, CLAP_EVENT_IS_LIVE);
    }

    /// Push a timed MIDI message, preserving its source flags.
    pub fn push_midi_with_flags(&mut self, msg: RawMidi, dialect: Dialect, time: u32, flags: u32) {
        match dialect {
            Dialect::None => {}
            Dialect::Midi => self.items.push(EvStorage {
                midi: clap_event_midi {
                    header: header_with_flags(
                        CLAP_EVENT_MIDI,
                        size_of::<clap_event_midi>(),
                        time,
                        flags,
                    ),
                    port_index: 0,
                    data: msg,
                },
            }),
            Dialect::Clap => {
                // ponytail: notes only. CC/pitchbend need the MIDI dialect —
                // a CLAP-only plugin gets them via its own param mapping.
                let status = msg[0] & 0xF0;
                let channel = i16::from(msg[0] & 0x0F);
                let key = i16::from(msg[1] & 0x7F);
                let vel = f64::from(msg[2] & 0x7F) / 127.0;
                let type_ = match status {
                    0x90 if msg[2] & 0x7F > 0 => CLAP_EVENT_NOTE_ON,
                    0x80 | 0x90 => CLAP_EVENT_NOTE_OFF,
                    _ => return,
                };
                self.items.push(EvStorage {
                    note: clap_event_note {
                        header: header_with_flags(type_, size_of::<clap_event_note>(), time, flags),
                        note_id: -1,
                        port_index: 0,
                        channel,
                        key,
                        velocity: if type_ == CLAP_EVENT_NOTE_ON {
                            vel.max(1.0 / 127.0)
                        } else {
                            vel
                        },
                    },
                });
            }
        }
    }

    /// Borrow as a `clap_input_events` vtable. Valid while `self` is not moved
    /// or mutated — i.e. for the duration of one `process()` call.
    #[must_use]
    pub fn as_input_events(&self) -> clap_input_events {
        clap_input_events {
            ctx: ptr::from_ref(self).cast::<c_void>().cast_mut(),
            size: Some(ev_size),
            get: Some(ev_get),
        }
    }
}

fn header(type_: u16, size: usize, time: u32) -> clap_event_header {
    header_with_flags(type_, size, time, CLAP_EVENT_IS_LIVE)
}

fn header_with_flags(type_: u16, size: usize, time: u32, flags: u32) -> clap_event_header {
    clap_event_header {
        size: size as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags,
    }
}

unsafe extern "C" fn ev_size(list: *const clap_input_events) -> u32 {
    let l = unsafe { &*(*list).ctx.cast::<EvList>() };
    l.items.len() as u32
}

unsafe extern "C" fn ev_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let l = unsafe { &*(*list).ctx.cast::<EvList>() };
    match l.items.get(index as usize) {
        Some(e) => ptr::from_ref(e).cast::<clap_event_header>(),
        None => ptr::null(),
    }
}

unsafe extern "C" fn ev_drop(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    true
}

/// Empty input event list for host→plugin calls like `params.flush`.
#[must_use]
pub fn empty_input_events() -> clap_input_events {
    clap_input_events {
        ctx: ptr::null_mut(),
        size: Some(empty_size),
        get: Some(empty_get),
    }
}

unsafe extern "C" fn empty_size(_: *const clap_input_events) -> u32 {
    0
}

unsafe extern "C" fn empty_get(_: *const clap_input_events, _: u32) -> *const clap_event_header {
    ptr::null()
}

/// Output event list that discards everything the plugin emits.
#[must_use]
pub fn sink_output_events() -> clap_output_events {
    clap_output_events {
        ctx: ptr::null_mut(),
        try_push: Some(ev_drop),
    }
}

fn capture_param_event(
    q: &ArrayQueue<PluginOutEvent>,
    h: &clap_event_header,
    header: *const clap_event_header,
) -> Option<bool> {
    let ev = match h.type_ {
        CLAP_EVENT_PARAM_VALUE => {
            if (h.size as usize) < size_of::<clap_event_param_value>() {
                return Some(true);
            }
            let p = unsafe { &*header.cast::<clap_event_param_value>() };
            PluginOutEvent::Value {
                id: p.param_id,
                value: p.value,
            }
        }
        CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            if (h.size as usize) < size_of::<clap_event_param_gesture>() {
                return Some(true);
            }
            let p = unsafe { &*header.cast::<clap_event_param_gesture>() };
            PluginOutEvent::GestureBegin { id: p.param_id }
        }
        CLAP_EVENT_PARAM_GESTURE_END => {
            if (h.size as usize) < size_of::<clap_event_param_gesture>() {
                return Some(true);
            }
            let p = unsafe { &*header.cast::<clap_event_param_gesture>() };
            PluginOutEvent::GestureEnd { id: p.param_id }
        }
        _ => return None,
    };
    Some(q.push(ev).is_ok())
}

unsafe extern "C" fn ev_try_push_capture(
    list: *const clap_output_events,
    header: *const clap_event_header,
) -> bool {
    if list.is_null() || header.is_null() {
        return false;
    }
    let q = unsafe { &*(*list).ctx.cast::<ArrayQueue<PluginOutEvent>>() };
    let h = unsafe { &*header };
    if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    capture_param_event(q, h, header).unwrap_or(true)
}

unsafe extern "C" fn ev_try_push_timed(
    list: *const clap_output_events,
    header: *const clap_event_header,
) -> bool {
    if list.is_null() || header.is_null() {
        return false;
    }
    let notes = unsafe { &mut *(*list).ctx.cast::<NoteOutList>() };
    let h = unsafe { &*header };
    if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    if !notes.params.is_null() {
        let q = unsafe { &*notes.params };
        if let Some(ok) = capture_param_event(q, h, header) {
            return ok;
        }
    }
    match h.type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE | CLAP_EVENT_NOTE_END => {
            if (h.size as usize) < size_of::<clap_event_note>() {
                return true;
            }
            let n = unsafe { &*header.cast::<clap_event_note>() };
            notes.try_push_note(PluginNoteOut::Note {
                type_: h.type_,
                time: h.time,
                flags: h.flags,
                note_id: n.note_id,
                port_index: n.port_index,
                channel: n.channel,
                key: n.key,
                velocity: n.velocity,
            })
        }
        CLAP_EVENT_MIDI => {
            if (h.size as usize) < size_of::<clap_event_midi>() {
                return true;
            }
            let m = unsafe { &*header.cast::<clap_event_midi>() };
            notes.try_push_note(PluginNoteOut::Midi {
                time: h.time,
                flags: h.flags,
                port_index: m.port_index,
                data: m.data,
            })
        }
        _ => true,
    }
}

/// Output list that forwards param value/gesture events into `q`.
/// `q` must outlive the `clap_process` / `params.flush` call.
#[must_use]
pub fn capturing_output_events(q: &Queue<PluginOutEvent>) -> clap_output_events {
    clap_output_events {
        // ArrayQueue behind the Arc — stable address for the call.
        ctx: Arc::as_ptr(q).cast::<c_void>().cast_mut(),
        try_push: Some(ev_try_push_capture),
    }
}

/// Output list that forwards note/MIDI events into `notes` and param events
/// into the queue bound by `NoteOutList::set_params`. `notes` must outlive
/// the `clap_process` call.
#[must_use]
pub fn capturing_timed_output_events(notes: &mut NoteOutList) -> clap_output_events {
    clap_output_events {
        ctx: ptr::from_mut(notes).cast(),
        try_push: Some(ev_try_push_timed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header offset 0 for every union variant, and dialect conversion.
    #[test]
    fn event_layout_and_dialect() {
        let mut l = EvList::with_capacity(8);
        l.push_param(7, 0.25, ptr::null_mut(), 0);
        l.push_midi([0x91, 60, 100], Dialect::Midi, 1);
        l.push_midi([0x91, 60, 100], Dialect::Clap, 2);
        l.push_midi([0x81, 60, 0], Dialect::Clap, 3);
        l.push_midi([0xB0, 7, 100], Dialect::Clap, 4); // CC: dropped
        l.push_midi([0x91, 60, 100], Dialect::None, 5); // no port: dropped

        let list = l.as_input_events();
        assert_eq!(unsafe { ev_size(&raw const list) }, 4);

        let types: Vec<u16> = (0..4)
            .map(|i| unsafe { (*ev_get(&raw const list, i)).type_ })
            .collect();
        assert_eq!(
            types,
            vec![
                CLAP_EVENT_PARAM_VALUE,
                CLAP_EVENT_MIDI,
                CLAP_EVENT_NOTE_ON,
                CLAP_EVENT_NOTE_OFF
            ]
        );

        // times survive the header punning → offset 0 holds
        let times: Vec<u32> = (0..4)
            .map(|i| unsafe { (*ev_get(&raw const list, i)).time })
            .collect();
        assert_eq!(times, vec![0, 1, 2, 3]);

        assert!(unsafe { ev_get(&raw const list, 4) }.is_null());
    }

    #[test]
    fn push_midi_with_flags_preserves_header_flags() {
        let mut l = EvList::with_capacity(4);
        l.push_midi([0x90, 60, 100], Dialect::Midi, 0);
        l.push_midi_with_flags([0x90, 61, 100], Dialect::Midi, 1, 0);
        l.push_midi_with_flags([0x90, 62, 100], Dialect::Clap, 2, 0);

        let list = l.as_input_events();
        let flags: Vec<u32> = (0..3)
            .map(|i| unsafe { (*ev_get(&raw const list, i)).flags })
            .collect();
        assert_eq!(flags, vec![CLAP_EVENT_IS_LIVE, 0, 0]);
    }

    #[test]
    fn capturing_output_keeps_param_value_and_gestures() {
        let q = queue::<PluginOutEvent>();
        let out = capturing_output_events(&q);

        let val = clap_event_param_value {
            header: header(
                CLAP_EVENT_PARAM_VALUE,
                size_of::<clap_event_param_value>(),
                0,
            ),
            param_id: 42,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.75,
        };
        let begin = clap_event_param_gesture {
            header: header(
                CLAP_EVENT_PARAM_GESTURE_BEGIN,
                size_of::<clap_event_param_gesture>(),
                0,
            ),
            param_id: 42,
        };
        let end = clap_event_param_gesture {
            header: header(
                CLAP_EVENT_PARAM_GESTURE_END,
                size_of::<clap_event_param_gesture>(),
                0,
            ),
            param_id: 42,
        };

        assert!(unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&val.header)) });
        assert!(unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&begin.header)) });
        assert!(unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&end.header)) });

        assert_eq!(
            q.pop(),
            Some(PluginOutEvent::Value {
                id: 42,
                value: 0.75
            })
        );
        assert_eq!(q.pop(), Some(PluginOutEvent::GestureBegin { id: 42 }));
        assert_eq!(q.pop(), Some(PluginOutEvent::GestureEnd { id: 42 }));
        assert!(q.pop().is_none());
    }

    #[test]
    fn capturing_output_ignores_unknown_types_but_returns_true() {
        let q = queue::<PluginOutEvent>();
        let out = capturing_output_events(&q);
        let note = clap_event_note {
            header: header(CLAP_EVENT_NOTE_ON, size_of::<clap_event_note>(), 0),
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
        };
        assert!(unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&note.header)) });
        assert!(q.pop().is_none());
    }

    #[test]
    fn capturing_output_returns_false_when_queue_is_full() {
        let q = queue::<PluginOutEvent>();
        let out = capturing_output_events(&q);
        let val = clap_event_param_value {
            header: header(
                CLAP_EVENT_PARAM_VALUE,
                size_of::<clap_event_param_value>(),
                0,
            ),
            param_id: 7,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.5,
        };
        for _ in 0..QUEUE_CAP {
            assert!(unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&val.header)) });
        }
        assert!(!unsafe { (out.try_push.unwrap())(&raw const out, ptr::from_ref(&val.header)) });
    }

    #[test]
    fn capturing_output_tolerates_undersized_and_null_headers() {
        let q = queue::<PluginOutEvent>();
        let out = capturing_output_events(&q);
        let undersized = clap_event_param_value {
            header: header(CLAP_EVENT_PARAM_VALUE, size_of::<clap_event_header>(), 0),
            param_id: 7,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.5,
        };
        // Size-Check schlägt zu: tolerant true, aber nichts gepusht.
        assert!(unsafe {
            (out.try_push.unwrap())(&raw const out, ptr::from_ref(&undersized.header))
        });
        assert!(q.pop().is_none());
        // Null-Header / -Liste: false.
        assert!(!unsafe { (out.try_push.unwrap())(&raw const out, ptr::null()) });
        let sink = sink_output_events();
        assert!(!unsafe { (out.try_push.unwrap())(&raw const sink, ptr::null()) });
    }

    fn push(out: &clap_output_events, header: *const clap_event_header) -> bool {
        unsafe { (out.try_push.unwrap())(out, header) }
    }

    #[allow(clippy::too_many_arguments)]
    fn note_event(
        type_: u16,
        time: u32,
        flags: u32,
        note_id: i32,
        port_index: i16,
        channel: i16,
        key: i16,
        velocity: f64,
    ) -> clap_event_note {
        clap_event_note {
            header: header_with_flags(type_, size_of::<clap_event_note>(), time, flags),
            note_id,
            port_index,
            channel,
            key,
            velocity,
        }
    }

    fn midi_event(time: u32, flags: u32, port_index: u16, data: [u8; 3]) -> clap_event_midi {
        clap_event_midi {
            header: header_with_flags(CLAP_EVENT_MIDI, size_of::<clap_event_midi>(), time, flags),
            port_index,
            data,
        }
    }

    fn param_value(id: clap_id, value: f64) -> clap_event_param_value {
        clap_event_param_value {
            header: header(
                CLAP_EVENT_PARAM_VALUE,
                size_of::<clap_event_param_value>(),
                0,
            ),
            param_id: id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        }
    }

    #[test]
    fn capturing_timed_output_keeps_note_and_midi() {
        let q = queue::<PluginOutEvent>();
        let mut notes = NoteOutList::with_capacity(MAX_NOTE_OUT);
        notes.set_params(&q);
        let out = capturing_timed_output_events(&mut notes);
        let on = note_event(CLAP_EVENT_NOTE_ON, 7, 0, 3, 1, 2, 60, 0.5);
        let off = note_event(CLAP_EVENT_NOTE_OFF, 9, 0, 3, 1, 2, 60, 0.0);
        let midi = midi_event(11, CLAP_EVENT_IS_LIVE, 4, [0x90, 64, 100]);
        let val = param_value(42, 0.25);
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert!(push(&out, ptr::from_ref(&off.header)));
        assert!(push(&out, ptr::from_ref(&midi.header)));
        assert!(push(&out, ptr::from_ref(&val.header)));
        assert_eq!(
            notes.as_slice(),
            &[
                PluginNoteOut::Note {
                    type_: CLAP_EVENT_NOTE_ON,
                    time: 7,
                    flags: 0,
                    note_id: 3,
                    port_index: 1,
                    channel: 2,
                    key: 60,
                    velocity: 0.5
                },
                PluginNoteOut::Note {
                    type_: CLAP_EVENT_NOTE_OFF,
                    time: 9,
                    flags: 0,
                    note_id: 3,
                    port_index: 1,
                    channel: 2,
                    key: 60,
                    velocity: 0.0
                },
                PluginNoteOut::Midi {
                    time: 11,
                    flags: CLAP_EVENT_IS_LIVE,
                    port_index: 4,
                    data: [0x90, 64, 100]
                },
            ]
        );
        assert_eq!(
            q.pop(),
            Some(PluginOutEvent::Value {
                id: 42,
                value: 0.25
            })
        );
    }

    #[test]
    fn capturing_timed_output_overflow_returns_false() {
        let q = queue::<PluginOutEvent>();
        let mut notes = NoteOutList::with_capacity(2);
        notes.set_params(&q);
        let out = capturing_timed_output_events(&mut notes);
        let on = note_event(CLAP_EVENT_NOTE_ON, 0, 0, -1, 0, 0, 60, 1.0);
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert!(!push(&out, ptr::from_ref(&on.header)));
        assert_eq!(notes.as_slice().len(), 2);
    }

    #[test]
    fn timed_capture_overflow_does_not_drop_params() {
        let q = queue::<PluginOutEvent>();
        let mut notes = NoteOutList::with_capacity(1);
        notes.set_params(&q);
        let out = capturing_timed_output_events(&mut notes);
        let on = note_event(CLAP_EVENT_NOTE_ON, 0, 0, -1, 0, 0, 60, 1.0);
        let val = param_value(7, 0.5);
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert!(!push(&out, ptr::from_ref(&on.header)));
        assert!(push(&out, ptr::from_ref(&val.header)));
        assert_eq!(q.pop(), Some(PluginOutEvent::Value { id: 7, value: 0.5 }));
    }

    #[test]
    fn note_out_list_ignores_spare_vec_capacity() {
        let q = queue::<PluginOutEvent>();
        let mut notes = NoteOutList::with_capacity(1);
        notes.items.reserve(32);
        assert!(
            notes.items.capacity() > 1,
            "need spare Vec capacity to pin the requested cap"
        );
        notes.set_params(&q);
        let out = capturing_timed_output_events(&mut notes);
        let on = note_event(CLAP_EVENT_NOTE_ON, 0, 0, -1, 0, 0, 60, 1.0);
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert!(!push(&out, ptr::from_ref(&on.header)));
        assert_eq!(notes.as_slice().len(), 1);
    }

    #[test]
    fn timed_capture_full_param_queue_still_keeps_notes() {
        let q = queue::<PluginOutEvent>();
        let mut notes = NoteOutList::with_capacity(MAX_NOTE_OUT);
        notes.set_params(&q);
        let out = capturing_timed_output_events(&mut notes);
        let val = param_value(7, 0.5);
        for _ in 0..QUEUE_CAP {
            assert!(push(&out, ptr::from_ref(&val.header)));
        }
        assert!(!push(&out, ptr::from_ref(&val.header)));
        let on = note_event(CLAP_EVENT_NOTE_ON, 3, 0, -1, 0, 0, 61, 1.0);
        assert!(push(&out, ptr::from_ref(&on.header)));
        assert_eq!(
            notes.as_slice(),
            &[PluginNoteOut::Note {
                type_: CLAP_EVENT_NOTE_ON,
                time: 3,
                flags: 0,
                note_id: -1,
                port_index: 0,
                channel: 0,
                key: 61,
                velocity: 1.0
            }]
        );
    }
}
