//! Factory preset list (discovery) + pull (load key → v1 state blob file).

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::fs;
use std::path::Path;
use std::ptr;

use clap_sys::ext::preset_load::{
    CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_plugin_preset_load,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::factory::preset_discovery::{
    CLAP_PRESET_DISCOVERY_FACTORY_ID, CLAP_PRESET_DISCOVERY_LOCATION_FILE,
    CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN, clap_preset_discovery_factory,
    clap_preset_discovery_indexer, clap_preset_discovery_location,
    clap_preset_discovery_metadata_receiver, clap_preset_discovery_provider,
};
use clap_sys::plugin::clap_plugin;
use clap_sys::stream::clap_ostream;
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;

use crate::loader::{self, Loader};

/// One factory preset as advertised by preset-discovery.
#[derive(Clone, Debug)]
pub struct ListedPreset {
    pub key: String,
    pub name: String,
}

struct ListCtx {
    presets: Vec<ListedPreset>,
}

unsafe extern "C" fn begin_preset(
    receiver: *const clap_preset_discovery_metadata_receiver,
    name: *const c_char,
    load_key: *const c_char,
) -> bool {
    if receiver.is_null() {
        return false;
    }
    let ctx = unsafe { &mut *(*receiver).receiver_data.cast::<ListCtx>() };
    let name = if name.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    };
    let key = if load_key.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(load_key) }
            .to_string_lossy()
            .into_owned()
    };
    ctx.presets.push(ListedPreset { key, name });
    true
}

unsafe extern "C" fn add_plugin_id(
    _receiver: *const clap_preset_discovery_metadata_receiver,
    _plugin_id: *const clap_universal_plugin_id,
) {
}

unsafe extern "C" fn set_flags(
    _receiver: *const clap_preset_discovery_metadata_receiver,
    _flags: u32,
) {
}

unsafe extern "C" fn declare_location(
    _indexer: *const clap_preset_discovery_indexer,
    _location: *const clap_preset_discovery_location,
) -> bool {
    true
}

/// Enumerate factory presets via `preset-discovery-factory/2` (no plugin instance).
pub fn list(loader: &Loader) -> Result<Vec<ListedPreset>, String> {
    let raw = loader.get_factory(CLAP_PRESET_DISCOVERY_FACTORY_ID);
    if raw.is_null() {
        return Err("no preset-discovery factory (plugin has empty factory_presets?)".into());
    }
    let factory = unsafe { &*raw.cast::<clap_preset_discovery_factory>() };
    let count = factory.count.ok_or("discovery.count is null")?;
    let get_desc = factory
        .get_descriptor
        .ok_or("discovery.get_descriptor is null")?;
    let create = factory.create.ok_or("discovery.create is null")?;

    let n = unsafe { count(factory) };
    if n == 0 {
        return Ok(Vec::new());
    }

    let name = CString::new("clap-host-rs").unwrap_or_default();
    let vendor = CString::new("lxndrbe").unwrap_or_default();
    let url = CString::new("https://github.com/lxndrbe/clap-host-rs").unwrap_or_default();
    let version = CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default();

    let indexer = clap_preset_discovery_indexer {
        clap_version: CLAP_VERSION,
        name: name.as_ptr(),
        vendor: vendor.as_ptr(),
        url: url.as_ptr(),
        version: version.as_ptr(),
        indexer_data: ptr::null_mut(),
        declare_filetype: None,
        declare_location: Some(declare_location),
        declare_soundpack: None,
        get_extension: None,
    };

    let mut out = Vec::new();
    for i in 0..n {
        let desc = unsafe { get_desc(factory, i) };
        if desc.is_null() {
            continue;
        }
        let id = unsafe { (*desc).id };
        if id.is_null() {
            continue;
        }
        let provider = unsafe { create(factory, &raw const indexer, id) };
        if provider.is_null() {
            continue;
        }
        let provider = unsafe { &*provider.cast::<clap_preset_discovery_provider>() };

        if let Some(init) = provider.init
            && !unsafe { init(provider) }
        {
            if let Some(destroy) = provider.destroy {
                unsafe { destroy(provider) };
            }
            continue;
        }

        let mut ctx = ListCtx {
            presets: Vec::new(),
        };
        let receiver = clap_preset_discovery_metadata_receiver {
            receiver_data: ptr::from_mut(&mut ctx).cast(),
            on_error: None,
            begin_preset: Some(begin_preset),
            add_plugin_id: Some(add_plugin_id),
            set_soundpack_id: None,
            set_flags: Some(set_flags),
            add_creator: None,
            set_description: None,
            set_timestamps: None,
            add_feature: None,
            add_extra_info: None,
        };

        if let Some(get_meta) = provider.get_metadata {
            let _ = unsafe {
                get_meta(
                    provider,
                    CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
                    ptr::null(),
                    &raw const receiver,
                )
            };
        }

        if let Some(destroy) = provider.destroy {
            unsafe { destroy(provider) };
        }
        out.append(&mut ctx.presets);
    }
    Ok(out)
}

struct OstreamCtx {
    buf: Vec<u8>,
}

unsafe extern "C" fn ostream_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = unsafe { &mut *(*stream).ctx.cast::<OstreamCtx>() };
    let n = size as usize;
    let slice = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
    ctx.buf.extend_from_slice(slice);
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Load factory preset `key` via `preset-load` and write the v1 state blob to `out`.
pub fn pull(plugin: *const clap_plugin, key: &str, out: &Path) -> Result<(), String> {
    let preset_ext =
        loader::plugin_ext(plugin, CLAP_EXT_PRESET_LOAD).ok_or("plugin has no clap.preset-load")?;
    let preset_ext = unsafe { &*preset_ext.cast::<clap_plugin_preset_load>() };
    let from_location = preset_ext
        .from_location
        .ok_or("preset-load.from_location is null")?;

    let key_c = CString::new(key).map_err(|e| e.to_string())?;
    if !unsafe {
        from_location(
            plugin,
            CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
            ptr::null(),
            key_c.as_ptr(),
        )
    } {
        return Err(format!("preset-load failed for key {key:?}"));
    }

    let state_ext = loader::plugin_ext(plugin, CLAP_EXT_STATE).ok_or("plugin has no clap.state")?;
    let state_ext = unsafe { &*state_ext.cast::<clap_plugin_state>() };
    let save = state_ext.save.ok_or("clap.state.save is null")?;

    let mut ctx = OstreamCtx { buf: Vec::new() };
    let stream = clap_ostream {
        ctx: ptr::from_mut(&mut ctx).cast(),
        write: Some(ostream_write),
    };
    if !unsafe { save(plugin, &raw const stream) } {
        return Err("clap.state.save returned false".into());
    }
    if ctx.buf.is_empty() {
        return Err("state blob is empty".into());
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    fs::write(out, &ctx.buf).map_err(|e| format!("write {}: {e}", out.display()))?;
    println!(
        "wrote {} bytes → {} (key={key})",
        ctx.buf.len(),
        out.display()
    );
    Ok(())
}

/// Does the plugin implement `clap.preset-load` (either version string)?
/// Unlike `pull` (preset.rs:200), which uses the final `/2` only, this also
/// tries the draft compat id — some shipped plugins still answer to it.
#[must_use]
pub fn load_file_available(plugin: *const clap_plugin) -> bool {
    loader::plugin_ext(plugin, CLAP_EXT_PRESET_LOAD)
        .or_else(|| loader::plugin_ext(plugin, CLAP_EXT_PRESET_LOAD_COMPAT))
        .is_some()
}

/// Ask the plugin to load a preset *file* via `preset-load` — for formats
/// only the plugin itself understands. This is the sibling of `pull`
/// (factory key → state blob); here the host hands over a file path and the
/// plugin applies the preset in place. Main thread only.
pub fn load_file(plugin: *const clap_plugin, path: &Path) -> Result<(), String> {
    let raw = loader::plugin_ext(plugin, CLAP_EXT_PRESET_LOAD)
        .or_else(|| loader::plugin_ext(plugin, CLAP_EXT_PRESET_LOAD_COMPAT))
        .ok_or("plugin has no clap.preset-load")?;
    let ext = unsafe { &*raw.cast::<clap_plugin_preset_load>() };
    let from_location = ext
        .from_location
        .ok_or("preset-load.from_location is null")?;
    // Non-UTF-8 paths degrade via lossy conversion; interior NUL fails cleanly.
    let path_c = CString::new(path.to_string_lossy().as_ref()).map_err(|e| e.to_string())?;
    if unsafe {
        from_location(
            plugin,
            CLAP_PRESET_DISCOVERY_LOCATION_FILE,
            path_c.as_ptr(),
            ptr::null(),
        )
    } {
        Ok(())
    } else {
        Err(format!("preset-load rejected {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::factory::preset_discovery::clap_preset_discovery_location_kind;
    use std::sync::Mutex;

    static CALL: Mutex<Option<(clap_preset_discovery_location_kind, String)>> = Mutex::new(None);

    unsafe extern "C" fn fake_from_location(
        _: *const clap_plugin,
        location_kind: clap_preset_discovery_location_kind,
        location: *const c_char,
        load_key: *const c_char,
    ) -> bool {
        let loc = if location.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(location) }
                .to_string_lossy()
                .into_owned()
        };
        *CALL.lock().unwrap() = Some((location_kind, loc));
        load_key.is_null()
    }

    static FAKE_PRESET_LOAD: clap_plugin_preset_load = clap_plugin_preset_load {
        from_location: Some(fake_from_location),
    };

    unsafe extern "C" fn fake_get_extension(
        _: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_PRESET_LOAD {
            ptr::from_ref(&FAKE_PRESET_LOAD).cast()
        } else {
            ptr::null()
        }
    }

    fn fake_plugin() -> clap_plugin {
        clap_plugin {
            get_extension: Some(fake_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        }
    }

    #[test]
    fn load_file_uses_file_location_and_null_load_key() {
        let plugin = fake_plugin();
        let path = Path::new("C:/tmp/init.clap-preset");
        assert!(load_file_available(&raw const plugin));
        assert!(load_file(&raw const plugin, path).is_ok());
        let (kind, loc) = CALL.lock().unwrap().take().expect("from_location called");
        assert_eq!(kind, CLAP_PRESET_DISCOVERY_LOCATION_FILE);
        assert_eq!(loc, "C:/tmp/init.clap-preset");
    }

    unsafe extern "C" fn null_get_extension(
        _: *const clap_plugin,
        _: *const c_char,
    ) -> *const c_void {
        ptr::null()
    }

    #[test]
    fn load_file_errors_without_extension() {
        let plugin = clap_plugin {
            get_extension: Some(null_get_extension),
            on_main_thread: None,
            ..unsafe { std::mem::zeroed() }
        };
        assert!(!load_file_available(&raw const plugin));
        let err = load_file(&raw const plugin, Path::new("x.clap-preset")).unwrap_err();
        assert!(err.contains("preset-load"));
    }
}
