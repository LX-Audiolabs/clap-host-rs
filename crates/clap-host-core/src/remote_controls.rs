//! Query side of `clap.remote-controls`: the plugin groups its most important
//! params into numbered "pages" (up to 8 param ids each). Host displays one
//! page at a time. See host.rs for the host-side extension (changed/suggest).

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::CStr;

use clap_sys::ext::remote_controls::{
    CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT, CLAP_REMOTE_CONTROLS_COUNT,
    clap_plugin_remote_controls,
};
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;

use crate::loader;

fn rc_ext(plugin: *const clap_plugin) -> Option<&'static clap_plugin_remote_controls> {
    let raw = loader::plugin_ext(plugin, CLAP_EXT_REMOTE_CONTROLS)
        .or_else(|| loader::plugin_ext(plugin, CLAP_EXT_REMOTE_CONTROLS_COMPAT))?;
    Some(unsafe { &*raw.cast::<clap_plugin_remote_controls>() })
}

/// One remote-controls page: a plugin-named group of up to 8 params.
#[derive(Debug, Clone, PartialEq)]
pub struct PageInfo {
    pub page_id: clap_id,
    pub name: String,
    pub params: Vec<clap_id>,
}

/// Does the plugin implement `clap.remote-controls`?
#[must_use]
pub fn available(plugin: *const clap_plugin) -> bool {
    rc_ext(plugin).is_some()
}

/// All pages the plugin exposes. Empty when the extension is missing, `count`
/// is absent, or a `get` call fails.
#[must_use]
pub fn pages(plugin: *const clap_plugin) -> Vec<PageInfo> {
    let Some(ext) = rc_ext(plugin) else { return Vec::new() };
    let (Some(count), Some(get)) = (ext.count, ext.get) else { return Vec::new() };
    let n = unsafe { count(plugin) };
    (0..n)
        .filter_map(|i| {
            let mut page = unsafe { std::mem::zeroed::<clap_sys::ext::remote_controls::clap_remote_controls_page>() };
            if !unsafe { get(plugin, i, &raw mut page) } {
                return None;
            }
            let name = unsafe { CStr::from_ptr(page.page_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let take = page.param_ids.iter().take_while(|&&id| id != 0).copied();
            let params: Vec<clap_id> = take.take(CLAP_REMOTE_CONTROLS_COUNT).collect();
            Some(PageInfo { page_id: page.page_id, name, params })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::ext::remote_controls::{
        CLAP_EXT_REMOTE_CONTROLS, clap_plugin_remote_controls, clap_remote_controls_page,
    };
    use clap_sys::plugin::clap_plugin;
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr;

    static FAKE_RC: clap_plugin_remote_controls = clap_plugin_remote_controls {
        count: Some(fake_count),
        get: Some(fake_get),
    };

    unsafe extern "C" fn fake_count(_: *const clap_plugin) -> u32 {
        2
    }

    unsafe extern "C" fn fake_get(
        _: *const clap_plugin,
        page_index: u32,
        page: *mut clap_remote_controls_page,
    ) -> bool {
        if page_index >= 2 {
            return false;
        }
        let page = unsafe { &mut *page };
        page.page_id = 100 + page_index;
        let name = format!("Page {page_index}");
        let bytes = name.as_bytes();
        page.page_name[..bytes.len()].copy_from_slice(bytes.iter().map(|&b| b as c_char).collect::<Vec<_>>().as_slice());
        page.param_ids = [10, 11, 12, 13, 14, 15, 16, 17];
        true
    }

    unsafe extern "C" fn fake_get_extension(
        _: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_REMOTE_CONTROLS {
            ptr::from_ref(&FAKE_RC).cast()
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
    fn available_and_pages_read_the_plugin_extension() {
        let plugin = fake_plugin();
        assert!(available(&raw const plugin));

        let pages = pages(&raw const plugin);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page_id, 100);
        assert_eq!(pages[0].name, "Page 0");
        assert_eq!(pages[0].params, vec![10, 11, 12, 13, 14, 15, 16, 17]);
        assert_eq!(pages[1].page_id, 101);
    }

    unsafe extern "C" fn fake_no_ext(_: *const clap_plugin, _: *const c_char) -> *const c_void {
        ptr::null()
    }

    #[test]
    fn missing_extension_means_no_pages() {
        let plugin = clap_plugin {
            get_extension: Some(fake_no_ext),
            ..unsafe { std::mem::zeroed() }
        };
        assert!(!available(&raw const plugin));
        assert!(pages(&raw const plugin).is_empty());
    }
}
