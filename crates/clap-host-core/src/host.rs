//! CLAP host callbacks: the `clap_host` vtable handed to every plugin, plus
//! the main/audio-thread bookkeeping and the host-side extensions.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use clap_sys::{
    ext::{
        gui::{CLAP_EXT_GUI, clap_host_gui},
        log::{
            CLAP_EXT_LOG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO,
            CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_host_log, clap_log_severity,
        },
        params::{
            CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags, clap_param_rescan_flags,
        },
        thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check},
    },
    host::clap_host,
    id::clap_id,
    plugin::clap_plugin,
    version::CLAP_VERSION,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

// ---------------------------------------------------------------------------
// Host extensions — log + thread_check (params/gui land in Phase 2)
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

/// Run the plugin's pending main-thread work. Call from the UI event loop.
pub fn pump_main_thread(plugin: *const clap_plugin) {
    if CALLBACK_REQUESTED.swap(false, Ordering::AcqRel)
        && let Some(cb) = unsafe { (*plugin).on_main_thread }
    {
        unsafe { cb(plugin) };
    }
}

/// True once per `request_restart` from the plugin.
pub fn take_restart_request() -> bool {
    RESTART_REQUESTED.swap(false, Ordering::AcqRel)
}

/// True once per `gui.closed` from the plugin.
pub fn take_gui_closed() -> bool {
    GUI_CLOSED.swap(false, Ordering::AcqRel)
}

unsafe extern "C" fn host_gui_resize_hints_changed(_: *const clap_host) {}
unsafe extern "C" fn host_gui_request_resize(_: *const clap_host, _: u32, _: u32) -> bool {
    // Floating windows size themselves; nothing for us to resize.
    false
}
unsafe extern "C" fn host_gui_request_show(_: *const clap_host) -> bool {
    false
}
unsafe extern "C" fn host_gui_request_hide(_: *const clap_host) -> bool {
    false
}
unsafe extern "C" fn host_gui_closed(_: *const clap_host, _was_destroyed: bool) {
    GUI_CLOSED.store(true, Ordering::Release);
}

unsafe extern "C" fn host_params_rescan(_: *const clap_host, _: clap_param_rescan_flags) {}
unsafe extern "C" fn host_params_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {
}
unsafe extern "C" fn host_params_request_flush(_: *const clap_host) {}

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
// The UI polls `params.get_value` instead of tracking output events, so these
// are accept-and-ignore. Plugins still expect the extension to exist.
static PARAMS_EXT: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
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
    let s = Box::leak(Box::new(Strings {
        name: CString::new("CLAP-Host-RS").unwrap(),
        vendor: CString::new("lxndrbe").unwrap(),
        url: CString::new("https://github.com/lxndrbe/CLAP-Host-RS").unwrap(),
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
