//! CLAP host callbacks: the `clap_host` vtable handed to every plugin, plus
//! the main/audio-thread bookkeeping and the host-side extensions.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use clap_sys::{
    ext::{
        gui::{CLAP_EXT_GUI, clap_host_gui},
        latency::{CLAP_EXT_LATENCY, clap_host_latency},
        log::{
            CLAP_EXT_LOG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO,
            CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_host_log, clap_log_severity,
        },
        note_name::{CLAP_EXT_NOTE_NAME, clap_host_note_name},
        params::{
            CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags, clap_param_rescan_flags,
        },
        preset_load::{CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_host_preset_load},
        remote_controls::{
            CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT, clap_host_remote_controls,
        },
        state::{CLAP_EXT_STATE, clap_host_state},
        tail::{CLAP_EXT_TAIL, clap_host_tail},
        thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check},
        timer_support::{
            CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support, clap_plugin_timer_support,
        },
    },
    factory::preset_discovery::clap_preset_discovery_location_kind,
    host::clap_host,
    id::clap_id,
    plugin::clap_plugin,
    version::CLAP_VERSION,
};
use crossbeam_queue::ArrayQueue;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

// Host extensions — log + thread_check + gui + params + state + timer +
// latency + tail + note-name + remote-controls + preset-load.
// ---------------------------------------------------------------------------

static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();
static AUDIO_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Called once from the cpal callback so `is_audio_thread` can answer honestly.
pub fn mark_audio_thread() {
    let _ = AUDIO_THREAD.set(std::thread::current().id());
}

unsafe extern "C" fn host_log(
    _: *const clap_host,
    severity: clap_log_severity,
    msg: *const c_char,
) {
    let text = if msg.is_null() {
        std::borrow::Cow::Borrowed("(null)")
    } else {
        unsafe { CStr::from_ptr(msg) }.to_string_lossy()
    };
    let tag = match severity {
        CLAP_LOG_INFO => "info",
        CLAP_LOG_WARNING => "warn",
        CLAP_LOG_ERROR => "error",
        CLAP_LOG_FATAL => "fatal",
        CLAP_LOG_HOST_MISBEHAVING => "host-misbehaving",
        CLAP_LOG_PLUGIN_MISBEHAVING => "plugin-misbehaving",
        _ => "debug",
    };
    eprintln!("[plugin/{tag}] {text}");
}

unsafe extern "C" fn host_is_main_thread(_: *const clap_host) -> bool {
    MAIN_THREAD.get() == Some(&std::thread::current().id())
}

unsafe extern "C" fn host_is_audio_thread(_: *const clap_host) -> bool {
    AUDIO_THREAD.get() == Some(&std::thread::current().id())
}

/// Set by `request_callback`; drained on the main thread by [`pump_main_thread`].
static CALLBACK_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by `request_restart`; the GUI reopens the audio session when it sees it.
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by the plugin's `gui.closed` callback — the window is gone.
static GUI_CLOSED: AtomicBool = AtomicBool::new(false);
/// Set by `gui.request_show` — GUI should make the plugin window visible.
static GUI_SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by `gui.request_hide` — GUI should hide the plugin window.
static GUI_HIDE_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by `params.rescan`/`params.clear`; the UI re-reads its param list.
static PARAMS_DIRTY: AtomicBool = AtomicBool::new(false);
/// Set by `params.request_flush`; drained on the audio thread by the engine.
static PARAMS_FLUSH_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by `state.mark_dirty` — the plugin's state changed since last save.
static STATE_DIRTY: AtomicBool = AtomicBool::new(false);
/// Set by `latency.changed`; re-query `clap_plugin_latency.get`.
static LATENCY_CHANGED: AtomicBool = AtomicBool::new(false);
/// Set by `tail.changed`; re-query `clap_plugin_tail.get`.
static TAIL_CHANGED: AtomicBool = AtomicBool::new(false);
/// Set by `note_name.changed`; re-query the plugin's note names.
static NOTE_NAME_CHANGED: AtomicBool = AtomicBool::new(false);
/// Set by `remote_controls.changed` — the plugin's page list changed.
static REMOTE_CONTROLS_DIRTY: AtomicBool = AtomicBool::new(false);
/// Latest `gui.request_resize` (w, h); drained by the GUI's main-loop timer.
/// Only meaningful while a plugin window is embedded — floating windows size
/// themselves, and the GUI discards a pending request in that case.
static RESIZE_REQUESTED: Mutex<Option<(u32, u32)>> = Mutex::new(None);
/// Set by `preset_load.loaded` — the plugin applied a preset; the UI re-reads
/// param values.
static PRESET_LOADED: AtomicBool = AtomicBool::new(false);
/// Set by `preset_load.on_error` — the plugin rejected a preset load.
static PRESET_LOAD_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Run the plugin's pending main-thread work. Call from the UI event loop.
pub fn pump_main_thread(plugin: *const clap_plugin) {
    if CALLBACK_REQUESTED.swap(false, Ordering::AcqRel)
        && let Some(cb) = unsafe { (*plugin).on_main_thread }
    {
        unsafe { cb(plugin) };
    }
    // Fire due timers here, on the main thread — never in the timer thread.
    fire_due_timers(plugin);
}

/// True once per `params.rescan`/`params.clear` from the plugin.
pub fn take_params_dirty() -> bool {
    PARAMS_DIRTY.swap(false, Ordering::AcqRel)
}

/// True once per `params.request_flush`; the engine drains this on the audio
/// thread and calls `clap_plugin_params.flush` there (as the spec requires).
pub fn take_params_flush_requested() -> bool {
    PARAMS_FLUSH_REQUESTED.swap(false, Ordering::AcqRel)
}

/// True once per `state.mark_dirty` from the plugin.
pub fn take_state_dirty() -> bool {
    STATE_DIRTY.swap(false, Ordering::AcqRel)
}

/// True once per `latency.changed` from the plugin.
pub fn take_latency_changed() -> bool {
    LATENCY_CHANGED.swap(false, Ordering::AcqRel)
}

/// True once per `tail.changed` from the plugin.
pub fn take_tail_changed() -> bool {
    TAIL_CHANGED.swap(false, Ordering::AcqRel)
}

/// True once per `note_name.changed` from the plugin.
pub fn take_note_name_changed() -> bool {
    NOTE_NAME_CHANGED.swap(false, Ordering::AcqRel)
}

/// True once per `remote_controls.changed` from the plugin.
pub fn take_remote_controls_dirty() -> bool {
    REMOTE_CONTROLS_DIRTY.swap(false, Ordering::AcqRel)
}

/// True once per `request_restart` from the plugin.
pub fn take_restart_request() -> bool {
    RESTART_REQUESTED.swap(false, Ordering::AcqRel)
}

/// True once per `gui.closed` from the plugin.
pub fn take_gui_closed() -> bool {
    GUI_CLOSED.swap(false, Ordering::AcqRel)
}

/// The plugin's latest `gui.request_resize`, once. The GUI applies it only
/// while a plugin window is embedded; floating windows size themselves.
pub fn take_requested_resize() -> Option<(u32, u32)> {
    RESIZE_REQUESTED.lock().ok()?.take()
}

/// True once per `preset_load.loaded` from the plugin.
pub fn take_preset_loaded() -> bool {
    PRESET_LOADED.swap(false, Ordering::AcqRel)
}

/// The plugin's last `preset_load.on_error` message, once.
pub fn take_preset_load_error() -> Option<String> {
    PRESET_LOAD_ERROR.lock().ok()?.take()
}

/// True once per `gui.request_show` from the plugin.
pub fn take_gui_show_requested() -> bool {
    GUI_SHOW_REQUESTED.swap(false, Ordering::AcqRel)
}

/// True once per `gui.request_hide` from the plugin.
pub fn take_gui_hide_requested() -> bool {
    GUI_HIDE_REQUESTED.swap(false, Ordering::AcqRel)
}

unsafe extern "C" fn host_gui_resize_hints_changed(_: *const clap_host) {}
unsafe extern "C" fn host_gui_request_resize(_: *const clap_host, width: u32, height: u32) -> bool {
    // [main-thread] per spec, so a plain Mutex is fine. Returning true means
    // "accepted" — the GUI drains the pending size on its next poll and
    // applies it to the embedded socket, or discards it when floating.
    if let Ok(mut slot) = RESIZE_REQUESTED.lock() {
        *slot = Some((width, height));
    }
    true
}
unsafe extern "C" fn host_gui_request_show(_: *const clap_host) -> bool {
    GUI_SHOW_REQUESTED.store(true, Ordering::Release);
    true
}
unsafe extern "C" fn host_gui_request_hide(_: *const clap_host) -> bool {
    GUI_HIDE_REQUESTED.store(true, Ordering::Release);
    true
}
unsafe extern "C" fn host_gui_closed(_: *const clap_host, _was_destroyed: bool) {
    GUI_CLOSED.store(true, Ordering::Release);
}
unsafe extern "C" fn host_preset_load_on_error(
    _: *const clap_host,
    _: clap_preset_discovery_location_kind,
    _: *const c_char,
    _: *const c_char,
    os_error: i32,
    msg: *const c_char,
) {
    let msg = if msg.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    };
    if let Ok(mut slot) = PRESET_LOAD_ERROR.lock() {
        *slot = Some(format!("preset load error: {msg} (os error {os_error})"));
    }
}
unsafe extern "C" fn host_preset_load_loaded(
    _: *const clap_host,
    _: clap_preset_discovery_location_kind,
    _: *const c_char,
    _: *const c_char,
) {
    PRESET_LOADED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_params_rescan(_: *const clap_host, _: clap_param_rescan_flags) {
    PARAMS_DIRTY.store(true, Ordering::Release);
}
unsafe extern "C" fn host_params_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {
    PARAMS_DIRTY.store(true, Ordering::Release);
}
unsafe extern "C" fn host_params_request_flush(_: *const clap_host) {
    PARAMS_FLUSH_REQUESTED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_state_mark_dirty(_: *const clap_host) {
    STATE_DIRTY.store(true, Ordering::Release);
}

unsafe extern "C" fn host_latency_changed(_: *const clap_host) {
    LATENCY_CHANGED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_tail_changed(_: *const clap_host) {
    TAIL_CHANGED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_note_name_changed(_: *const clap_host) {
    NOTE_NAME_CHANGED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_remote_controls_changed(_: *const clap_host) {
    REMOTE_CONTROLS_DIRTY.store(true, Ordering::Release);
}
unsafe extern "C" fn host_remote_controls_suggest_page(_: *const clap_host, _: clap_id) {
    // ponytail: no-op — we don't scroll-to-page a physical control surface;
    // add an atomic + GUI honor-step if a plugin ever relies on it.
}

// ---------------------------------------------------------------------------
// Timers — one background thread ticks every 5 ms and queues due ids;
// `pump_main_thread` (main thread) delivers them via `timer_support.on_timer`.
// ---------------------------------------------------------------------------

static TIMERS: Mutex<Vec<(clap_id, u32, Instant)>> = Mutex::new(Vec::new());
static TIMER_DUE: LazyLock<ArrayQueue<clap_id>> = LazyLock::new(|| ArrayQueue::new(256));
static NEXT_TIMER_ID: AtomicU32 = AtomicU32::new(1);
static TIMER_THREAD: Once = Once::new();

const TIMER_TICK: Duration = Duration::from_millis(5);

fn spawn_timer_thread() {
    TIMER_THREAD.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(TIMER_TICK);
                let now = Instant::now();
                let Ok(mut timers) = TIMERS.lock() else {
                    continue;
                };
                for (id, period_ms, last) in timers.iter_mut() {
                    if now.duration_since(*last) >= Duration::from_millis(u64::from(*period_ms)) {
                        *last = now;
                        // Full queue means the main thread is stuck; drop, don't block.
                        let _ = TIMER_DUE.push(*id);
                    }
                }
            }
        });
    });
}

/// Register a timer ticking every `period_ms`; returns its id, or `None` if
/// `period_ms` is 0. Ids start at 1.
pub fn request_timer(period_ms: u32) -> Option<clap_id> {
    if period_ms == 0 {
        return None;
    }
    let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut timers) = TIMERS.lock() {
        timers.push((id, period_ms, Instant::now()));
    }
    Some(id)
}

/// Remove a timer. Returns false if the id was not registered.
pub fn cancel_timer(timer_id: clap_id) -> bool {
    let Ok(mut timers) = TIMERS.lock() else {
        return false;
    };
    let before = timers.len();
    timers.retain(|(id, _, _)| *id != timer_id);
    timers.len() != before
}

/// True if `timer_id` is still registered (stale due-ids are dropped).
fn timer_registered(timer_id: clap_id) -> bool {
    TIMERS
        .lock()
        .is_ok_and(|timers| timers.iter().any(|(id, _, _)| *id == timer_id))
}

fn fire_due_timers(plugin: *const clap_plugin) {
    if TIMER_DUE.is_empty() {
        return;
    }
    let Some(raw) = crate::loader::plugin_ext(plugin, CLAP_EXT_TIMER_SUPPORT) else {
        // Plugin can't take timers; drain the queue so it doesn't grow.
        while TIMER_DUE.pop().is_some() {}
        return;
    };
    let timers = unsafe { &*raw.cast::<clap_plugin_timer_support>() };
    let Some(on_timer) = timers.on_timer else {
        while TIMER_DUE.pop().is_some() {}
        return;
    };
    while let Some(id) = TIMER_DUE.pop() {
        if timer_registered(id) {
            unsafe { on_timer(plugin, id) };
        }
    }
}

unsafe extern "C" fn host_timer_register(
    _: *const clap_host,
    period_ms: u32,
    timer_id: *mut clap_id,
) -> bool {
    if timer_id.is_null() {
        return false;
    }
    match request_timer(period_ms) {
        Some(id) => {
            unsafe { *timer_id = id };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_timer_unregister(_: *const clap_host, timer_id: clap_id) -> bool {
    cancel_timer(timer_id)
}

static LOG_EXT: clap_host_log = clap_host_log {
    log: Some(host_log),
};
static GUI_EXT: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};
static PARAMS_EXT: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};
static STATE_EXT: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};
static PRESET_LOAD_EXT: clap_host_preset_load = clap_host_preset_load {
    on_error: Some(host_preset_load_on_error),
    loaded: Some(host_preset_load_loaded),
};
static TIMER_EXT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_timer_register),
    unregister_timer: Some(host_timer_unregister),
};
static LATENCY_EXT: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};
static TAIL_EXT: clap_host_tail = clap_host_tail {
    changed: Some(host_tail_changed),
};
static NOTE_NAME_EXT: clap_host_note_name = clap_host_note_name {
    changed: Some(host_note_name_changed),
};
static REMOTE_CONTROLS_EXT: clap_host_remote_controls = clap_host_remote_controls {
    changed: Some(host_remote_controls_changed),
    suggest_page: Some(host_remote_controls_suggest_page),
};
static THREAD_CHECK_EXT: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_is_main_thread),
    is_audio_thread: Some(host_is_audio_thread),
};

unsafe extern "C" fn host_get_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_LOG {
        return ptr::from_ref(&LOG_EXT).cast();
    }
    if id == CLAP_EXT_THREAD_CHECK {
        return ptr::from_ref(&THREAD_CHECK_EXT).cast();
    }
    if id == CLAP_EXT_GUI {
        return ptr::from_ref(&GUI_EXT).cast();
    }
    if id == CLAP_EXT_PARAMS {
        return ptr::from_ref(&PARAMS_EXT).cast();
    }
    if id == CLAP_EXT_STATE {
        return ptr::from_ref(&STATE_EXT).cast();
    }
    if id == CLAP_EXT_TIMER_SUPPORT {
        return ptr::from_ref(&TIMER_EXT).cast();
    }
    if id == CLAP_EXT_LATENCY {
        return ptr::from_ref(&LATENCY_EXT).cast();
    }
    if id == CLAP_EXT_TAIL {
        return ptr::from_ref(&TAIL_EXT).cast();
    }
    if id == CLAP_EXT_NOTE_NAME {
        return ptr::from_ref(&NOTE_NAME_EXT).cast();
    }
    if id == CLAP_EXT_REMOTE_CONTROLS || id == CLAP_EXT_REMOTE_CONTROLS_COMPAT {
        return ptr::from_ref(&REMOTE_CONTROLS_EXT).cast();
    }
    if id == CLAP_EXT_PRESET_LOAD || id == CLAP_EXT_PRESET_LOAD_COMPAT {
        return ptr::from_ref(&PRESET_LOAD_EXT).cast();
    }
    ptr::null()
}
unsafe extern "C" fn host_request_restart(_: *const clap_host) {
    RESTART_REQUESTED.store(true, Ordering::Release);
}
unsafe extern "C" fn host_request_process(_: *const clap_host) {}
unsafe extern "C" fn host_request_callback(_: *const clap_host) {
    CALLBACK_REQUESTED.store(true, Ordering::Release);
}

/// Leak a `clap_host` with stable identity strings. Call once, from the main thread.
pub fn make_host() -> &'static clap_host {
    struct Strings {
        name: CString,
        vendor: CString,
        url: CString,
        version: CString,
    }
    let _ = MAIN_THREAD.set(std::thread::current().id());
    spawn_timer_thread();
    let s = Box::leak(Box::new(Strings {
        name: CString::new("CLAP-Host-RS").unwrap(),
        vendor: CString::new("lxndrbe").unwrap(),
        url: CString::new("https://github.com/lxndrbe/clap-host-rs").unwrap(),
        version: CString::new(env!("CARGO_PKG_VERSION")).unwrap(),
    }));
    Box::leak(Box::new(clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: s.name.as_ptr(),
        vendor: s.vendor.as_ptr(),
        url: s.url.as_ptr(),
        version: s.version.as_ptr(),
        get_extension: Some(host_get_extension),
        request_restart: Some(host_request_restart),
        request_process: Some(host_request_process),
        request_callback: Some(host_request_callback),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_FILE;

    static TIMER_FIRES: AtomicU32 = AtomicU32::new(0);
    static OFF_THREAD_FIRES: AtomicU32 = AtomicU32::new(0);
    static PUMP_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

    static PLUGIN_TIMER: clap_plugin_timer_support = clap_plugin_timer_support {
        on_timer: Some(fake_on_timer),
    };

    unsafe extern "C" fn fake_on_timer(_: *const clap_plugin, _: clap_id) {
        TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
        // on_timer must run on the thread that called pump_main_thread.
        let caller = PUMP_THREAD.lock().ok().and_then(|g| *g);
        if caller.is_some_and(|t| t != std::thread::current().id()) {
            OFF_THREAD_FIRES.fetch_add(1, Ordering::Relaxed);
        }
    }

    unsafe extern "C" fn fake_get_extension(
        _: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_TIMER_SUPPORT {
            ptr::from_ref(&PLUGIN_TIMER).cast()
        } else {
            ptr::null()
        }
    }

    #[test]
    fn dirty_and_changed_flags_are_one_shot() {
        unsafe { host_params_rescan(ptr::null(), 0) };
        assert!(take_params_dirty());
        assert!(!take_params_dirty());

        unsafe { host_params_request_flush(ptr::null()) };
        assert!(take_params_flush_requested());
        assert!(!take_params_flush_requested());

        unsafe { host_state_mark_dirty(ptr::null()) };
        assert!(take_state_dirty());
        assert!(!take_state_dirty());

        unsafe { host_latency_changed(ptr::null()) };
        assert!(take_latency_changed());
        assert!(!take_latency_changed());

        unsafe { host_tail_changed(ptr::null()) };
        assert!(take_tail_changed());
        assert!(!take_tail_changed());

        unsafe { host_note_name_changed(ptr::null()) };
        assert!(take_note_name_changed());
        assert!(!take_note_name_changed());

        unsafe { host_remote_controls_changed(ptr::null()) };
        assert!(take_remote_controls_dirty());
        assert!(!take_remote_controls_dirty());

        unsafe { host_gui_request_resize(ptr::null(), 640, 480) };
        assert_eq!(take_requested_resize(), Some((640, 480)));
        assert_eq!(take_requested_resize(), None);

        unsafe { host_gui_request_show(ptr::null()) };
        assert!(take_gui_show_requested());
        assert!(!take_gui_show_requested());

        unsafe { host_gui_request_hide(ptr::null()) };
        assert!(take_gui_hide_requested());
        assert!(!take_gui_hide_requested());

        unsafe {
            host_preset_load_loaded(
                ptr::null(),
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                ptr::null(),
                ptr::null(),
            )
        };
        assert!(take_preset_loaded());
        assert!(!take_preset_loaded());

        unsafe {
            host_preset_load_on_error(
                ptr::null(),
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                ptr::null(),
                ptr::null(),
                42,
                c"file not found".as_ptr(),
            )
        };
        let err = take_preset_load_error().expect("on_error message pending");
        assert!(err.contains("file not found"));
        assert!(take_preset_load_error().is_none());
    }

    #[test]
    fn timer_registers_ticks_and_fires_on_main_thread() {
        make_host(); // records this thread as main + spawns the tick thread
        let plugin = clap_plugin {
            get_extension: Some(fake_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        };

        let id = request_timer(5).expect("period 5 is valid");
        assert!(!cancel_timer(id + 1_000_000));
        if let Ok(mut slot) = PUMP_THREAD.lock() {
            *slot = Some(std::thread::current().id());
        }
        std::thread::sleep(Duration::from_millis(60));
        pump_main_thread(&plugin);

        assert!(
            TIMER_FIRES.load(Ordering::Relaxed) > 0,
            "due timer ids should be delivered via on_timer"
        );
        assert_eq!(OFF_THREAD_FIRES.load(Ordering::Relaxed), 0);
        assert!(cancel_timer(id));
        while TIMER_DUE.pop().is_some() {}
    }

    #[test]
    fn host_get_extension_serves_every_extension() {
        make_host();
        let host = make_host();
        let get = host.get_extension.unwrap();
        for id in [
            CLAP_EXT_LOG,
            CLAP_EXT_THREAD_CHECK,
            CLAP_EXT_GUI,
            CLAP_EXT_PARAMS,
            CLAP_EXT_STATE,
            CLAP_EXT_TIMER_SUPPORT,
            CLAP_EXT_LATENCY,
            CLAP_EXT_TAIL,
            CLAP_EXT_NOTE_NAME,
            CLAP_EXT_REMOTE_CONTROLS,
            CLAP_EXT_REMOTE_CONTROLS_COMPAT,
            CLAP_EXT_PRESET_LOAD,
            CLAP_EXT_PRESET_LOAD_COMPAT,
        ] {
            assert!(
                !unsafe { get(host, id.as_ptr()) }.is_null(),
                "missing {id:?}"
            );
        }
        let unknown = c"clap.definitely-not-real";
        assert!(unsafe { get(host, unknown.as_ptr()) }.is_null());
    }
}
