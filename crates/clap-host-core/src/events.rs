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
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_OFF,
        CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END,
        CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_midi, clap_event_note,
        clap_event_param_gesture, clap_event_param_value, clap_input_events, clap_output_events,
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
        match dialect {
            Dialect::None => {}
            Dialect::Midi => self.items.push(EvStorage {
                midi: clap_event_midi {
                    header: header(CLAP_EVENT_MIDI, size_of::<clap_event_midi>(), time),
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
                        header: header(type_, size_of::<clap_event_note>(), time),
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
    clap_event_header {
        size: size as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: CLAP_EVENT_IS_LIVE,
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
    let ev = match h.type_ {
        CLAP_EVENT_PARAM_VALUE => {
            if (h.size as usize) < size_of::<clap_event_param_value>() {
                return true;
            }
            let p = unsafe { &*header.cast::<clap_event_param_value>() };
            PluginOutEvent::Value {
                id: p.param_id,
                value: p.value,
            }
        }
        CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            if (h.size as usize) < size_of::<clap_event_param_gesture>() {
                return true;
            }
            let p = unsafe { &*header.cast::<clap_event_param_gesture>() };
            PluginOutEvent::GestureBegin { id: p.param_id }
        }
        CLAP_EVENT_PARAM_GESTURE_END => {
            if (h.size as usize) < size_of::<clap_event_param_gesture>() {
                return true;
            }
            let p = unsafe { &*header.cast::<clap_event_param_gesture>() };
            PluginOutEvent::GestureEnd { id: p.param_id }
        }
        _ => return true,
    };
    q.push(ev).is_ok()
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
}
