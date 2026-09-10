//! CLAP plugin loader: dlopen .clap, factory, plugin lifecycle, param listing.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use clap_sys::{
    entry::clap_plugin_entry,
    ext::{
        audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports},
        note_ports::{
            CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI,
            clap_note_port_info, clap_plugin_note_ports,
        },
        params::{CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN, clap_param_info, clap_plugin_params},
    },
    factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory},
    host::clap_host,
    id::clap_id,
    plugin::{clap_plugin, clap_plugin_descriptor},
};
use std::ffi::{CStr, CString, c_char, c_void};

use crate::events::{Dialect, EvList, sink_output_events};

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

type GetFactoryFn = unsafe extern "C" fn(*const c_char) -> *const c_void;

pub struct Loader {
    _lib: libloading::Library,
    factory: *const clap_plugin_factory,
    get_factory_fn: GetFactoryFn,
    deinit: Option<unsafe extern "C" fn()>,
}

// Safety: we never share the Loader across threads.
unsafe impl Send for Loader {}

impl Loader {
    /// dlopen `path`, call `clap_entry.init`, and locate the plugin factory.
    pub unsafe fn open(path: &str) -> Result<Self, String> {
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("dlopen: {e}"))?;

        let sym: libloading::Symbol<*const clap_plugin_entry> =
            unsafe { lib.get(b"clap_entry\0") }.map_err(|e| format!("clap_entry symbol: {e}"))?;
        let entry = unsafe { &**sym };

        let init = entry.init.ok_or("entry.init is null")?;
        let get_factory = entry.get_factory.ok_or("entry.get_factory is null")?;

        let path_c = CString::new(path).map_err(|e| e.to_string())?;
        if !unsafe { init(path_c.as_ptr()) } {
            return Err("entry.init returned false".into());
        }

        let factory_raw = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
        if factory_raw.is_null() {
            return Err("get_factory: no plugin factory".into());
        }

        Ok(Self {
            _lib: lib,
            factory: factory_raw.cast::<clap_plugin_factory>(),
            get_factory_fn: get_factory,
            deinit: entry.deinit,
        })
    }

    /// Raw `clap_entry.get_factory` — used for preset-discovery (and future factories).
    #[must_use]
    pub fn get_factory(&self, id: &CStr) -> *const c_void {
        unsafe { (self.get_factory_fn)(id.as_ptr()) }
    }

    pub fn plugin_count(&self) -> u32 {
        unsafe { &*self.factory }
            .get_plugin_count
            .map_or(0, |f| unsafe { f(self.factory) })
    }

    pub fn descriptor(&self, index: u32) -> Option<&clap_plugin_descriptor> {
        let f = unsafe { &*self.factory };
        f.get_plugin_descriptor.and_then(|g| {
            let p = unsafe { g(self.factory, index) };
            if p.is_null() {
                None
            } else {
                Some(unsafe { &*p })
            }
        })
    }

    /// Create the first plugin whose id matches `want_id` (or the first plugin
    /// if `want_id` is `None`).
    pub fn create(
        &self,
        host: *const clap_host,
        want_id: Option<&str>,
    ) -> Result<*const clap_plugin, String> {
        let f = unsafe { &*self.factory };
        let create_fn = f.create_plugin.ok_or("factory.create_plugin is null")?;
        for i in 0..self.plugin_count() {
            let Some(desc) = self.descriptor(i) else {
                continue;
            };
            if desc.id.is_null() {
                continue;
            }
            let id_cstr = unsafe { CStr::from_ptr(desc.id) };
            if let Some(want) = want_id
                && id_cstr.to_str() != Ok(want)
            {
                continue;
            }
            let p = unsafe { create_fn(self.factory, host, desc.id) };
            if !p.is_null() {
                return Ok(p);
            }
        }
        Err(match want_id {
            Some(id) => format!("plugin id={id:?} not found"),
            None => "factory.create_plugin returned null for all plugins".into(),
        })
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        if let Some(deinit) = self.deinit {
            unsafe { deinit() };
        }
    }
}

// ---------------------------------------------------------------------------
// Param listing
// ---------------------------------------------------------------------------

/// Fetch a plugin extension by id. Returns `None` if unsupported.
pub fn plugin_ext(plugin: *const clap_plugin, id: &CStr) -> Option<*const c_void> {
    let get_ext = unsafe { (*plugin).get_extension }?;
    let raw = unsafe { get_ext(plugin, id.as_ptr()) };
    if raw.is_null() { None } else { Some(raw) }
}

fn params_ext(plugin: *const clap_plugin) -> Option<&'static clap_plugin_params> {
    plugin_ext(plugin, CLAP_EXT_PARAMS).map(|p| unsafe { &*p.cast::<clap_plugin_params>() })
}

/// Channel count of every audio port on one side (`is_input` picks the side).
/// The host must hand `process()` a buffer for *each* declared port, so this
/// returns all of them, not just the main one.
pub fn audio_port_channels(plugin: *const clap_plugin, is_input: bool) -> Vec<u32> {
    let Some(ext) = plugin_ext(plugin, CLAP_EXT_AUDIO_PORTS) else {
        return Vec::new();
    };
    let ports = unsafe { &*ext.cast::<clap_plugin_audio_ports>() };
    let count = ports.count.map_or(0, |f| unsafe { f(plugin, is_input) });
    let Some(get) = ports.get else {
        return Vec::new();
    };
    let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
    (0..count)
        .map(|i| {
            if unsafe { get(plugin, i, is_input, &raw mut info) } {
                info.channel_count
            } else {
                0
            }
        })
        .collect()
}

/// Note dialect the plugin's first *input* note port prefers.
pub fn note_dialect(plugin: *const clap_plugin) -> Dialect {
    let Some(ext) = plugin_ext(plugin, CLAP_EXT_NOTE_PORTS) else {
        return Dialect::None;
    };
    let ports = unsafe { &*ext.cast::<clap_plugin_note_ports>() };
    if ports.count.map_or(0, |f| unsafe { f(plugin, true) }) == 0 {
        return Dialect::None;
    }
    let Some(get) = ports.get else {
        return Dialect::None;
    };
    let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
    if !unsafe { get(plugin, 0, true, &raw mut info) } {
        return Dialect::None;
    }
    // Prefer what the port asks for; fall back to anything it supports.
    for d in [info.preferred_dialect, info.supported_dialects] {
        if d & CLAP_NOTE_DIALECT_MIDI != 0 {
            return Dialect::Midi;
        }
        if d & CLAP_NOTE_DIALECT_CLAP != 0 {
            return Dialect::Clap;
        }
    }
    Dialect::None
}

/// Call `params.flush` with an empty input list — the audio-thread half of
/// `params.request_flush`. No-op when the plugin has no params extension.
pub fn flush_params(plugin: *const clap_plugin) {
    let Some(params) = params_ext(plugin) else {
        return;
    };
    let Some(flush) = params.flush else { return };
    let in_ev = crate::events::empty_input_events();
    let out_ev = sink_output_events();
    unsafe { flush(plugin, &raw const in_ev, &raw const out_ev) };
}

/// Set one param on the *deactivated* plugin via `params.flush` (main thread).
pub fn set_param(plugin: *const clap_plugin, id: clap_id, value: f64) -> Result<(), String> {
    let params = params_ext(plugin).ok_or("plugin has no clap.params extension")?;
    let flush = params.flush.ok_or("clap.params has no flush")?;
    // Echo the plugin's own cookie back when we know it (self::params — the
    // local `params` above is the extension vtable).
    let cookie = self::params(plugin)
        .iter()
        .find(|p| p.id == id)
        .map_or(std::ptr::null_mut(), |p| p.cookie);
    let mut evs = EvList::with_capacity(1);
    evs.push_param(id, value, cookie, 0);
    let in_ev = evs.as_input_events();
    let out_ev = sink_output_events();
    unsafe { flush(plugin, &raw const in_ev, &raw const out_ev) };
    Ok(())
}

/// One visible parameter, as the UI and the CLI listing both want it.
#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: clap_id,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub value: f64,
    /// The plugin's own cookie for this param (`clap_param_info.cookie`) —
    /// echo it back in param events so the plugin can skip the id lookup.
    pub cookie: *mut core::ffi::c_void,
}

/// Every non-hidden parameter, with its current value. Main thread only.
#[must_use]
pub fn params(plugin: *const clap_plugin) -> Vec<ParamInfo> {
    let Some(params) = params_ext(plugin) else {
        return Vec::new();
    };
    let (Some(get_info), count) = (
        params.get_info,
        params.count.map_or(0, |f| unsafe { f(plugin) }),
    ) else {
        return Vec::new();
    };
    let mut info: clap_param_info = unsafe { std::mem::zeroed() };
    (0..count)
        .filter_map(|i| {
            if !unsafe { get_info(plugin, i, &raw mut info) }
                || info.flags & CLAP_PARAM_IS_HIDDEN != 0
            {
                return None;
            }
            Some(ParamInfo {
                id: info.id,
                name: unsafe { CStr::from_ptr(info.name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
                min: info.min_value,
                max: info.max_value,
                value: param_value(plugin, info.id).unwrap_or(f64::NAN),
                cookie: info.cookie,
            })
        })
        .collect()
}

/// Current value of one parameter. Main thread only.
#[must_use]
pub fn param_value(plugin: *const clap_plugin, id: clap_id) -> Option<f64> {
    let get = params_ext(plugin)?.get_value?;
    let mut v = 0.0f64;
    unsafe { get(plugin, id, &raw mut v) }.then_some(v)
}

/// The plugin's own formatting for a value ("-6.0 dB"). Falls back to the
/// plain number when the plugin has no `value_to_text`.
#[must_use]
pub fn param_text(plugin: *const clap_plugin, id: clap_id, value: f64) -> String {
    let fallback = || format!("{value:.2}");
    let Some(to_text) = params_ext(plugin).and_then(|p| p.value_to_text) else {
        return fallback();
    };
    let mut buf = [0u8; 64];
    if !unsafe {
        to_text(
            plugin,
            id,
            value,
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len() as u32,
        )
    } {
        return fallback();
    }
    CStr::from_bytes_until_nul(&buf)
        .map_or_else(|_| fallback(), |s| s.to_string_lossy().into_owned())
}

/// Newtype so `*const clap_plugin` can cross thread boundaries into the cpal callback.
#[derive(Copy, Clone)]
pub struct PluginPtr(pub *const clap_plugin);
unsafe impl Send for PluginPtr {}
