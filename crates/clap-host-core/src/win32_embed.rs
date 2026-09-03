//! Windows only: embed the plugin's own window as a `WS_CHILD` "socket"
//! inside ours, via CLAP's `gui.set_parent` — instead of floating it
//! in its own top-level window (`plugin_gui.rs`).
//!
//! Vital and Surge XT answer `is_api_supported(floating=true) == false` —
//! they can only be embedded, which is why this path exists beside the
//! floating one. Ported 1:1 from AURA's `win32_embed.rs` (proven there).

#![cfg(windows)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// Window sizes here are plugin editor dimensions — nowhere near i32::MAX/2.
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ptr;
use std::sync::OnceLock;

use clap_sys::ext::gui::{CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_plugin_gui, clap_window};
use clap_sys::plugin::clap_plugin;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, WNDCLASSW, SM_CXSCREEN,
    SM_CYSCREEN, WS_CHILD, WS_VISIBLE,
};
use windows_sys::core::PCWSTR;

use crate::loader;

fn gui_ext(plugin: *const clap_plugin) -> Option<&'static clap_plugin_gui> {
    loader::plugin_ext(plugin, CLAP_EXT_GUI).map(|p| unsafe { &*p.cast::<clap_plugin_gui>() })
}

/// Can this plugin embed into a Win32 `HWND` we provide?
#[must_use]
pub fn supports_embedded(plugin: *const clap_plugin) -> bool {
    gui_ext(plugin).is_some_and(|g| {
        g.is_api_supported
            .is_some_and(|f| unsafe { f(plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) })
    })
}

/// The plugin's preferred editor size (`gui.get_size`), 400x300 fallback.
#[must_use]
pub fn preferred_size(plugin: *const clap_plugin) -> (u32, u32) {
    let mut size = (400u32, 300u32);
    if let Some(get_size) = gui_ext(plugin).and_then(|g| g.get_size) {
        let (mut w, mut h) = (0u32, 0u32);
        if unsafe { get_size(plugin, &raw mut w, &raw mut h) } && w > 0 && h > 0 {
            size = (w, h);
        }
    }
    size
}

/// Ask the plugin to resize its editor. May be refused; re-read with
/// [`preferred_size`] afterwards to learn the size the plugin actually took.
#[must_use]
pub fn request_size(plugin: *const clap_plugin, width: u32, height: u32) -> bool {
    gui_ext(plugin)
        .and_then(|g| g.set_size)
        .is_some_and(|f| unsafe { f(plugin, width, height) })
}

/// Size of the monitor `hwnd` sits on (work area = screen minus taskbar),
/// physical px. Falls back to the primary screen's full size.
#[must_use]
pub fn monitor_work_area(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: Default::default(),
            rcWork: Default::default(),
            dwFlags: 0,
        };
        if !monitor.is_null() && GetMonitorInfoW(monitor, &raw mut info) != 0 {
            return (
                info.rcWork.right - info.rcWork.left,
                info.rcWork.bottom - info.rcWork.top,
            );
        }
        (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
    }
}

/// Window class for the plain socket window the plugin embeds into. Registered
/// once; `DefWindowProcW` is enough — the plugin's own child window handles
/// its own input, and we don't paint anything into this window ourselves.
fn socket_class() -> PCWSTR {
    static ATOM: OnceLock<u16> = OnceLock::new();
    let atom = *ATOM.get_or_init(|| {
        let name: Vec<u16> = "ClapHostEmbedSocket\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: unsafe { GetModuleHandleW(ptr::null()) },
            hIcon: ptr::null_mut(),
            hCursor: ptr::null_mut(),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: name.as_ptr(),
        };
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::RegisterClassW(&raw const wc) }
    });
    assert!(atom != 0, "RegisterClassW(ClapHostEmbedSocket) failed");
    // MAKEINTATOM: an atom is passed as a class name through the low word of a pointer.
    atom as usize as PCWSTR
}

/// A plugin GUI embedded via `WS_CHILD` + `set_parent`. Dropping it hides and
/// destroys the plugin's editor, then the socket window.
pub struct EmbeddedGui {
    plugin: *const clap_plugin,
    socket: HWND,
}

impl EmbeddedGui {
    /// Create the plugin's editor, cap its size to `max_size` (asking the
    /// plugin to shrink via `gui.set_size` if its preferred size doesn't
    /// fit), then create the socket at `(x, y)` in `parent`'s client
    /// coordinates and embed the plugin into it. Returns the embedded GUI and
    /// the editor size the socket was actually created with. Main thread only.
    ///
    /// Size negotiation happens *after* `gui.create` — some plugins (Vital)
    /// only answer `gui.get_size` truthfully once created.
    pub fn open(
        plugin: *const clap_plugin,
        parent: HWND,
        x: i32,
        y: i32,
        max_size: Option<(u32, u32)>,
    ) -> Result<(Self, (u32, u32)), String> {
        let gui = gui_ext(plugin).ok_or("plugin has no clap.gui extension")?;
        if !supports_embedded(plugin) {
            return Err("plugin does not support embedded (non-floating) GUIs".into());
        }
        let create = gui.create.ok_or("clap.gui has no create")?;
        if !unsafe { create(plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) } {
            return Err("gui.create returned false".into());
        }

        let (mut w, mut h) = preferred_size(plugin);
        if let Some((max_w, max_h)) = max_size
            && (w > max_w || h > max_h)
        {
            w = w.min(max_w);
            h = h.min(max_h);
            let _accepted = request_size(plugin, w, h);
            let (nw, nh) = preferred_size(plugin);
            w = nw.min(max_w);
            h = nh.min(max_h);
        }

        let socket = unsafe {
            CreateWindowExW(
                0,
                socket_class(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE,
                x,
                y,
                w as i32,
                h as i32,
                parent,
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            )
        };
        if socket.is_null() {
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(plugin) };
            }
            return Err("CreateWindowExW failed".into());
        }

        let window = clap_window {
            api: CLAP_WINDOW_API_WIN32.as_ptr(),
            specific: clap_sys::ext::gui::clap_window_handle { win32: socket },
        };
        let parented = gui
            .set_parent
            .is_some_and(|f| unsafe { f(plugin, &raw const window) });
        if !parented {
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(plugin) };
            }
            unsafe { DestroyWindow(socket) };
            return Err("gui.set_parent returned false".into());
        }

        let shown = gui.show.is_none_or(|f| unsafe { f(plugin) });
        if !shown {
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(plugin) };
            }
            unsafe { DestroyWindow(socket) };
            return Err("gui.show returned false".into());
        }

        Ok((Self { plugin, socket }, (w, h)))
    }

    /// The socket's current size in physical pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        let mut r = windows_sys::Win32::Foundation::RECT::default();
        // GetClientRect always sets left=top=0 — immune to RDP off-screen positions.
        let ok = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(self.socket, &raw mut r)
        };
        if ok != 0 {
            (r.right as u32, r.bottom as u32)
        } else {
            (0, 0)
        }
    }
}

impl Drop for EmbeddedGui {
    fn drop(&mut self) {
        if let Some(gui) = gui_ext(self.plugin) {
            if let Some(hide) = gui.hide {
                unsafe { hide(self.plugin) };
            }
            if let Some(destroy) = gui.destroy {
                unsafe { destroy(self.plugin) };
            }
        }
        unsafe { DestroyWindow(self.socket) };
    }
}
