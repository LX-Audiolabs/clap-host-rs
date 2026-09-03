//! Slint shell: a window over the same plugin instance the CLI would run.
//!
//! Everything here runs on the main thread — CLAP's main-thread class. Audio
//! runs in the cpal callback and is reached only through the lock-free queues.

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_precision_loss)]
// Param ids and Slint model indices are small positive ints in practice —
// wrap/sign-loss on a u32<->i32 param id or a ComboBox row index isn't a real risk.
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::too_many_lines)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use clap_host_core::audio::{self, Session};
use clap_host_core::clap_sys::plugin::clap_plugin;
use clap_host_core::events::{self, Queue, RawMidi, UiEvent};
use clap_host_core::host::{
    pump_main_thread, take_gui_closed, take_restart_request, take_state_dirty,
};
use clap_host_core::loader::{self, ParamInfo, PluginPtr};
use clap_host_core::midi;
use clap_host_core::plugin_gui::{FloatingGui, supports_floating};
use clap_host_core::state;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// How often the UI re-reads param values and runs the plugin's main-thread work.
const POLL: Duration = Duration::from_millis(50);

/// Either kind of plugin window we can have open. Neither variant's payload is
/// read again after construction — closing/cleanup happens entirely through
/// `Drop`. Windows-only: embedded GUIs need a Win32 parent HWND (Slint+winit).
#[allow(dead_code)]
enum PluginWindow {
    Floating(FloatingGui),
    #[cfg(windows)]
    Embedded(clap_host_core::win32_embed::EmbeddedGui),
}

/// Where the embedded plugin socket sits in our window's client area (physical
/// px). The window grows to make room for it after a successful open.
#[cfg(windows)]
const EMBED_X: i32 = 16;
#[cfg(windows)]
const EMBED_Y: i32 = 480;

/// Raw HWND of our own top-level window, to embed a plugin's GUI into.
#[cfg(windows)]
fn parent_hwnd(ui: &HostWindow) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let slint_handle = ui.window().window_handle();
    let handle = HasWindowHandle::window_handle(&slint_handle).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as windows_sys::Win32::Foundation::HWND),
        _ => None,
    }
}

/// Everything the callbacks mutate. Single-threaded, hence `RefCell`.
struct Host {
    plugin: PluginPtr,
    /// Input device the audio session was opened with; kept so a device switch
    /// rebuilds the session on the same input.
    input_name: Option<String>,
    midi_q: Queue<RawMidi>,
    ui_q: Queue<UiEvent>,
    session: Option<Session>,
    gui: Option<PluginWindow>,
    midi_conn: Option<midir::MidiInputConnection<()>>,
    /// `params()` is asked once; only values are polled after that.
    params: Vec<ParamInfo>,
    /// Remote-controls pages (empty = plugin has no clap.remote-controls).
    remote_pages: Vec<clap_host_core::remote_controls::PageInfo>,
    remote_page: usize,
    /// Fixed state path: the plugin file with a `.state.bin` suffix.
    state_path: PathBuf,
}

impl Host {
    fn plugin(&self) -> *const clap_plugin {
        self.plugin.0
    }
}

fn param_rows(host: &Host) -> Vec<ParamRow> {
    host.params
        .iter()
        .map(|p| {
            let value = loader::param_value(host.plugin(), p.id).unwrap_or(p.value);
            ParamRow {
                id: p.id as i32,
                name: SharedString::from(&p.name),
                value: value as f32,
                minimum: p.min as f32,
                maximum: p.max as f32,
                text: SharedString::from(loader::param_text(host.plugin(), p.id, value)),
            }
        })
        .collect()
}

/// ParamRows for the current remote-controls page; params the plugin did not
/// expose in its regular param list are skipped.
fn remote_rows(host: &Host) -> Vec<ParamRow> {
    let Some(page) = host.remote_pages.get(host.remote_page) else {
        return Vec::new();
    };
    page.params
        .iter()
        .filter_map(|&id| host.params.iter().find(|p| p.id == id))
        .map(|p| {
            let value = loader::param_value(host.plugin(), p.id).unwrap_or(p.value);
            ParamRow {
                id: p.id as i32,
                name: SharedString::from(&p.name),
                value: value as f32,
                minimum: p.min as f32,
                maximum: p.max as f32,
                text: SharedString::from(loader::param_text(host.plugin(), p.id, value)),
            }
        })
        .collect()
}

/// Page name + indices for the Slint header (page/page-count are 0 when empty).
fn remote_nav(host: &Host) -> (SharedString, i32, i32) {
    match host.remote_pages.get(host.remote_page) {
        Some(page) => (
            SharedString::from(&page.name),
            host.remote_page as i32,
            host.remote_pages.len() as i32,
        ),
        None => (SharedString::new(), 0, 0),
    }
}

/// Open the Slint shell. Returns when the window closes.
pub fn run(
    plugin: *const clap_plugin,
    name: &str,
    id: &str,
    midi_in: Option<&str>,
    input_name: Option<&str>,
    plugin_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let ui = HostWindow::new()?;

    let mut state_path = plugin_path.to_path_buf();
    state_path.as_mut_os_string().push(".state.bin");

    let host = Rc::new(RefCell::new(Host {
        plugin: PluginPtr(plugin),
        input_name: input_name.map(str::to_owned),
        midi_q: events::queue(),
        ui_q: events::queue(),
        session: None,
        gui: None,
        midi_conn: None,
        params: loader::params(plugin),
        remote_pages: if clap_host_core::remote_controls::available(plugin) {
            clap_host_core::remote_controls::pages(plugin)
        } else {
            Vec::new()
        },
        remote_page: 0,
        state_path,
    }));

    ui.set_plugin_name(SharedString::from(name));
    ui.set_plugin_id(SharedString::from(id));
    #[cfg(windows)]
    let can_embed = clap_host_core::win32_embed::supports_embedded(plugin);
    #[cfg(not(windows))]
    let can_embed = false;
    ui.set_gui_available(supports_floating(plugin) || can_embed);

    let devices = audio::output_devices();
    let ports = midi::port_names();
    ui.set_audio_devices(ModelRc::new(VecModel::from(
        devices.iter().map(SharedString::from).collect::<Vec<_>>(),
    )));
    ui.set_midi_ports(ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from("(none)"))
            .chain(ports.iter().map(SharedString::from))
            .collect::<Vec<_>>(),
    )));

    let in_devices = audio::input_devices();
    ui.set_audio_in_devices(ModelRc::new(VecModel::from(
        in_devices.iter().map(SharedString::from).collect::<Vec<_>>(),
    )));
    // Only plugins with audio *input* ports consume a capture device.
    ui.set_has_audio_in(!loader::audio_port_channels(plugin, true).is_empty());
    if let Some(i) = host
        .borrow()
        .input_name
        .as_deref()
        .and_then(|n| in_devices.iter().position(|d| d == n))
    {
        ui.set_audio_in_index(i as i32);
    }

    let params_model = Rc::new(VecModel::from(param_rows(&host.borrow())));
    ui.set_params(ModelRc::from(Rc::clone(&params_model)));
    let remote_model = Rc::new(VecModel::from(remote_rows(&host.borrow())));
    ui.set_remote_params(ModelRc::from(Rc::clone(&remote_model)));
    {
        let (name, page, count) = remote_nav(&host.borrow());
        ui.set_remote_available(!host.borrow().remote_pages.is_empty());
        ui.set_remote_page_name(name);
        ui.set_remote_page(page);
        ui.set_remote_page_count(count);
    }

    // Start on the default device, and on the requested MIDI port if given.
    start_audio(&ui, &host, None);
    if let Some(want) = midi_in {
        let idx = ports
            .iter()
            .position(|p| p.to_lowercase().contains(&want.to_lowercase()));
        match idx {
            Some(i) => {
                ui.set_midi_index(i as i32 + 1);
                start_midi(&ui, &host, Some(&ports[i]));
            }
            None => ui.set_midi_status(SharedString::from(format!("no port matching {want:?}"))),
        }
    }

    {
        let host = Rc::clone(&host);
        ui.on_param_changed(move |id, value| {
            let h = host.borrow();
            let id = id as u32;
            let value = f64::from(value);
            // While a stream runs the audio thread applies it; otherwise flush directly.
            if h.session.is_some() {
                let _ = h.ui_q.push(UiEvent::Param { id, value });
            } else if let Err(e) = loader::set_param(h.plugin(), id, value) {
                eprintln!("warn: set param {id}: {e}");
            }
        });
    }
    {
        let host = Rc::clone(&host);
        ui.on_note_on(move |key| push_note(&host.borrow(), 0x90, key));
    }
    {
        let host = Rc::clone(&host);
        ui.on_note_off(move |key| push_note(&host.borrow(), 0x80, key));
    }
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_audio_device_changed(move |index| {
            let Some(ui) = ui_w.upgrade() else { return };
            let name = ui.get_audio_devices().row_data(index as usize);
            start_audio(&ui, &host, name.as_deref());
        });
    }
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_audio_in_changed(move |index| {
            let Some(ui) = ui_w.upgrade() else { return };
            let in_name = ui.get_audio_in_devices().row_data(index as usize);
            host.borrow_mut().input_name = in_name.as_deref().map(str::to_owned);
            // Keep the current output device; only the capture side switches.
            let out_name = ui
                .get_audio_devices()
                .row_data(ui.get_audio_index().max(0) as usize);
            start_audio(&ui, &host, out_name.as_deref());
        });
    }
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_midi_port_changed(move |index| {
            let Some(ui) = ui_w.upgrade() else { return };
            // Row 0 is "(none)".
            let name = if index <= 0 {
                None
            } else {
                ui.get_midi_ports().row_data(index as usize)
            };
            start_midi(&ui, &host, name.as_deref());
        });
    }
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_toggle_gui(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut h = host.borrow_mut();
            if h.gui.take().is_some() {
                ui.set_gui_open(false);
                return;
            }
            let plugin = h.plugin();

            match FloatingGui::open(plugin, ui.get_plugin_name().as_str()) {
                Ok(gui) => {
                    h.gui = Some(PluginWindow::Floating(gui));
                    ui.set_gui_open(true);
                }
                Err(float_err) => {
                    #[cfg(windows)]
                    if clap_host_core::win32_embed::supports_embedded(plugin)
                        && let Some(parent) = parent_hwnd(&ui)
                    {
                        match clap_host_core::win32_embed::EmbeddedGui::open(
                            plugin, parent, EMBED_X, EMBED_Y,
                        ) {
                            Ok(embedded) => {
                                let (w, h_px) = embedded.size();
                                let cur = ui.window().size();
                                ui.window().set_size(slint::WindowSize::Physical(
                                    slint::PhysicalSize::new(
                                        cur.width.max(EMBED_X as u32 + w + 16),
                                        cur.height.max(EMBED_Y as u32 + h_px + 16),
                                    ),
                                ));
                                h.gui = Some(PluginWindow::Embedded(embedded));
                                ui.set_gui_open(true);
                            }
                            Err(embed_err) => {
                                ui.set_log_text(SharedString::from(format!(
                                    "plugin GUI: {float_err}; embed: {embed_err}"
                                )));
                            }
                        }
                    }
                    #[cfg(not(windows))]
                    ui.set_log_text(SharedString::from(format!("plugin GUI: {float_err}")));
                }
            }
        });
    }

    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_remote_prev(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut h = host.borrow_mut();
            h.remote_page = h.remote_page.saturating_sub(1);
            ui.set_remote_page(h.remote_page as i32);
            if let Some(page) = h.remote_pages.get(h.remote_page) {
                ui.set_remote_page_name(SharedString::from(&page.name));
            }
        });
    }
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_remote_next(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let mut h = host.borrow_mut();
            if h.remote_page + 1 < h.remote_pages.len() {
                h.remote_page += 1;
            }
            ui.set_remote_page(h.remote_page as i32);
            if let Some(page) = h.remote_pages.get(h.remote_page) {
                ui.set_remote_page_name(SharedString::from(&page.name));
            }
        });
    }
    {
        let host = Rc::clone(&host);
        ui.on_remote_param_changed(move |id, value| {
            let h = host.borrow();
            let id = id as u32;
            let value = f64::from(value);
            // Same event path as on_param_changed — a page is just another view.
            if h.session.is_some() {
                let _ = h.ui_q.push(UiEvent::Param { id, value });
            } else if let Err(e) = loader::set_param(h.plugin(), id, value) {
                eprintln!("warn: set param {id}: {e}");
            }
        });
    }

    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        ui.on_save_state(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let h = host.borrow();
            match state::save(h.plugin(), &h.state_path) {
                Ok(()) => {
                    // Drain a mark_dirty that raced the save, then clear the dot.
                    let _ = take_state_dirty();
                    ui.set_state_dirty(false);
                    ui.set_log_text(SharedString::from(format!(
                        "state saved to {}",
                        h.state_path.display()
                    )));
                }
                Err(e) => ui.set_log_text(SharedString::from(format!("save state: {e}"))),
            }
        });
    }
    {
        let (host, ui_w, params_model) = (Rc::clone(&host), ui.as_weak(), Rc::clone(&params_model));
        ui.on_load_state(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let h = host.borrow();
            match state::load(h.plugin(), &h.state_path) {
                Ok(()) => {
                    let _ = take_state_dirty();
                    ui.set_state_dirty(false);
                    ui.set_log_text(SharedString::from(format!(
                        "state loaded from {}",
                        h.state_path.display()
                    )));
                    drop(h);
                    // load() changed the values; refresh the whole model at once.
                    params_model.set_vec(param_rows(&host.borrow()));
                }
                Err(e) => ui.set_log_text(SharedString::from(format!("load state: {e}"))),
            }
        });
    }

    // One timer drives everything the plugin expects from the main thread.
    let timer = slint::Timer::default();
    {
        let (host, ui_w) = (Rc::clone(&host), ui.as_weak());
        let params_model = Rc::clone(&params_model);
        let remote_model = Rc::clone(&remote_model);
        timer.start(slint::TimerMode::Repeated, POLL, move || {
            let Some(ui) = ui_w.upgrade() else { return };
            pump_main_thread(host.borrow().plugin());

            if take_gui_closed() {
                host.borrow_mut().gui = None;
                ui.set_gui_open(false);
            }
            if take_restart_request() {
                let name = ui
                    .get_audio_devices()
                    .row_data(ui.get_audio_index().max(0) as usize);
                start_audio(&ui, &host, name.as_deref());
            }
            if take_state_dirty() {
                ui.set_state_dirty(true);
            }
            if clap_host_core::host::take_remote_controls_dirty() {
                let mut h = host.borrow_mut();
                h.remote_pages = clap_host_core::remote_controls::pages(h.plugin());
                if h.remote_page >= h.remote_pages.len() {
                    h.remote_page = h.remote_pages.len().saturating_sub(1);
                }
                let (name, page, count) = remote_nav(&h);
                ui.set_remote_available(!h.remote_pages.is_empty());
                ui.set_remote_page_name(name);
                ui.set_remote_page(page);
                ui.set_remote_page_count(count);
            }
            let rows = remote_rows(&host.borrow());
            // Pages can shrink or grow on a switch; set_row_data silently
            // no-ops out of range, so resync the length before diff-updating.
            if remote_model.row_count() != rows.len() {
                remote_model.set_vec(rows);
            } else {
                for (i, row) in rows.into_iter().enumerate() {
                    if remote_model.row_data(i).as_ref() != Some(&row) {
                        remote_model.set_row_data(i, row);
                    }
                }
            }

            // ponytail: polling get_value instead of reading the plugin's output
            // events; 20 Hz is enough for sliders and needs no return queue.
            let rows = param_rows(&host.borrow());
            for (i, row) in rows.into_iter().enumerate() {
                if params_model.row_data(i).as_ref() != Some(&row) {
                    params_model.set_row_data(i, row);
                }
            }
        });
    }

    ui.run()?;
    // Drop order: plugin window, then stream+activation, then the MIDI port.
    let mut h = host.borrow_mut();
    h.gui = None;
    h.session = None;
    h.midi_conn = None;
    Ok(())
}

fn push_note(host: &Host, status: u8, key: i32) {
    let Ok(key) = u8::try_from(key) else { return };
    if key > 127 {
        return;
    }
    let velocity = if status == 0x90 { 100 } else { 0 };
    let _ = host.ui_q.push(UiEvent::Midi([status, key, velocity]));
}

fn start_audio(ui: &HostWindow, host: &Rc<RefCell<Host>>, device: Option<&str>) {
    let mut h = host.borrow_mut();
    // Drop the old session first: it deactivates the plugin, which must happen
    // before activate() runs again.
    h.session = None;
    let (plugin, input_name, midi_q, ui_q) = (
        h.plugin(),
        h.input_name.clone(),
        Queue::clone(&h.midi_q),
        Queue::clone(&h.ui_q),
    );
    match audio::open(plugin, device, input_name.as_deref(), midi_q, ui_q) {
        Ok(s) => {
            ui.set_audio_status(SharedString::from(format!(
                "{} Hz · {} ch · ports in {:?} / out {:?} · notes {:?}",
                s.sample_rate, s.device_channels, s.in_ports, s.out_ports, s.dialect
            )));
            h.session = Some(s);
        }
        Err(e) => ui.set_audio_status(SharedString::from(format!("stopped — {e}"))),
    }
}

fn start_midi(ui: &HostWindow, host: &Rc<RefCell<Host>>, port: Option<&str>) {
    let mut h = host.borrow_mut();
    h.midi_conn = None;
    let Some(port) = port else {
        ui.set_midi_status(SharedString::from("no MIDI input"));
        return;
    };
    let q = Queue::clone(&h.midi_q);
    match midi::open(Some(port), &q) {
        Ok(conn) => {
            h.midi_conn = Some(conn);
            ui.set_midi_status(SharedString::from(format!("listening on {port}")));
        }
        Err(e) => ui.set_midi_status(SharedString::from(e)),
    }
}
