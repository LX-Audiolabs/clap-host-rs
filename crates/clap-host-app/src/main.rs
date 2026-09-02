//! clap-host-rs — minimal CLAP host.
//!
//! Phase 1 is the CLI (params, presets, MIDI, audio).
//!
//! Usage:
//!   clap-host-rs --plugin <path.clap> [--id <clap-id>] [--list-params]
//!                [--list-presets] [--pull-preset <key> --out <file>]
//!                [--set <id>=<val>]... [--load-state <file>]
//!                [--save-state <file>] [--play [--output-device <name>]
//!                [--input-device <name>]] [--list-midi] [--midi-in <name>] |
//!                --gui | --scan | --list-devices

#![allow(clippy::missing_safety_doc)]

mod cli;
mod gui;

use std::ffi::CStr;
use std::path::Path;

use clap_host_core::audio;
use clap_host_core::events::{self, Queue, RawMidi, UiEvent};
use clap_host_core::host::{make_host, pump_main_thread};
use clap_host_core::loader::{self, Loader};
use clap_host_core::midi;
use clap_host_core::preset;
use clap_host_core::scan;
use clap_host_core::state;

use clap_host_core::clap_sys::plugin::clap_plugin;

fn main() {
    let args = cli::parse_args();

    if args.list_midi {
        print_midi_ports();
    }

    if args.list_devices {
        print_devices();
    }

    if args.scan {
        print_scan();
    }

    let Some(path) = args.plugin_path.clone() else {
        // --scan, --list-midi and --list-devices alone are valid standalone queries.
        let standalone = (args.scan || args.list_midi || args.list_devices)
            && !args.play
            && !args.gui
            && !args.list_params
            && !args.list_presets
            && args.pull_preset.is_none()
            && args.sets.is_empty();
        if !standalone {
            eprintln!("error: --plugin <path.clap> is required");
            eprintln!("{}", cli::USAGE);
            std::process::exit(1);
        }
        return;
    };

    let loader = unsafe { Loader::open(&path) }.unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    // Always list plugins in the file.
    let n = loader.plugin_count();
    println!("found {n} plugin(s) in {path}");
    for idx in 0..n {
        if let Some(d) = loader.descriptor(idx) {
            let name = unsafe { CStr::from_ptr(d.name) }.to_string_lossy();
            let id = unsafe { CStr::from_ptr(d.id) }.to_string_lossy();
            println!("  [{idx}] {name}  id={id}");
        }
    }

    if args.list_presets {
        print_presets(&loader);
    }

    let needs_instance = args.list_params
        || args.play
        || args.gui
        || !args.sets.is_empty()
        || args.pull_preset.is_some()
        || args.load_state.is_some()
        || args.save_state.is_some();
    if !needs_instance {
        return;
    }

    if args.pull_preset.is_some() && args.pull_out.is_none() {
        eprintln!("error: --pull-preset needs --out <file>");
        std::process::exit(1);
    }

    let host = make_host();
    let plugin = loader
        .create(host, args.plugin_id.as_deref())
        .unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1);
        });

    if let Some(init) = unsafe { (*plugin).init }
        && !unsafe { init(plugin) }
    {
        eprintln!("error: plugin.init returned false");
        std::process::exit(1);
    }

    // --load-state right after init, so --set and --play see the restored values.
    if let Some(path) = &args.load_state
        && let Err(e) = state::load(plugin, path)
    {
        eprintln!("error: load state: {e}");
        std::process::exit(1);
    }

    // Applied while deactivated, so the values are in effect before activate().
    for (id, value) in &args.sets {
        match loader::set_param(plugin, *id, *value) {
            Ok(()) => println!("set param {id} = {value}"),
            Err(e) => eprintln!("error: set param {id}: {e}"),
        }
    }

    if let (Some(key), Some(out)) = (&args.pull_preset, &args.pull_out)
        && let Err(e) = preset::pull(plugin, key, out)
    {
        eprintln!("error: pull preset: {e}");
        std::process::exit(1);
    }

    // --save-state after --set/preset pull, before --play/--list-params.
    if let Some(path) = &args.save_state
        && let Err(e) = state::save(plugin, path)
    {
        eprintln!("error: save state: {e}");
        std::process::exit(1);
    }

    if args.list_params {
        print_params(plugin);
    }

    if args.gui {
        let (name, id) = unsafe { plugin_name_id(plugin) };
        if let Err(e) = gui::run(
            plugin,
            &name,
            &id,
            args.midi_in.as_deref(),
            args.input_device.as_deref(),
            Path::new(&path),
        ) {
            eprintln!("error: gui: {e}");
        }
        // gui::run returns when the window closes; fall through to destroy.
    }

    if args.play {
        let midi_q = events::queue();
        // Connection must outlive the stream: dropping it closes the MIDI port.
        let _conn = midi::open(args.midi_in.as_deref(), &midi_q)
            .inspect_err(|e| eprintln!("warn: {e} — running without MIDI"))
            .ok();
        run_play(
            plugin,
            midi_q,
            events::queue(),
            args.output_device.as_deref(),
            args.input_device.as_deref(),
        );
        // run_play loops forever; if it returns, fall through to destroy.
    }

    if let Some(destroy) = unsafe { (*plugin).destroy } {
        unsafe { destroy(plugin) };
    }
}

/// Every visible parameter with its current value, in the plugin's own
/// text formatting. Ported from aura-host's `loader::list_params`.
fn print_params(plugin: *const clap_plugin) {
    let all = loader::params(plugin);
    if all.is_empty() {
        println!("  (no params)");
        return;
    }
    println!("{} param(s):", all.len());
    for (i, p) in all.iter().enumerate() {
        println!(
            "  [{i}] id={} {} = {:.4} ({})  [{:.4}..{:.4}]",
            p.id,
            p.name,
            p.value,
            loader::param_text(plugin, p.id, p.value),
            p.min,
            p.max
        );
    }
}

/// Factory presets from the plugin's preset-discovery provider.
fn print_presets(loader: &Loader) {
    match preset::list(loader) {
        Ok(presets) if presets.is_empty() => {
            println!("0 factory preset(s)");
        }
        Ok(presets) => {
            println!("{} factory preset(s):", presets.len());
            for (i, p) in presets.iter().enumerate() {
                println!("  [{i}] {}  key={}", p.name, p.key);
            }
        }
        Err(e) => {
            eprintln!("error: list presets: {e}");
            std::process::exit(1);
        }
    }
}

fn print_midi_ports() {
    let names = midi::port_names();
    println!("{} MIDI input port(s):", names.len());
    for (i, name) in names.iter().enumerate() {
        println!("  [{i}] {name}");
    }
}

/// CLI `--list-devices`: audio output, audio input and MIDI ports.
fn print_devices() {
    let outs = audio::output_devices();
    println!("{} audio output device(s):", outs.len());
    for (i, name) in outs.iter().enumerate() {
        println!("  [{i}] {name}");
    }
    let ins = audio::input_devices();
    println!("{} audio input device(s):", ins.len());
    for (i, name) in ins.iter().enumerate() {
        println!("  [{i}] {name}");
    }
    print_midi_ports();
}

/// CLI `--scan`: walk the OS standard CLAP dirs and list every plugin.
/// A broken .clap is a warning; the scan keeps going.
fn print_scan() {
    let paths = scan::scan();
    if paths.is_empty() {
        println!("no CLAP plugins found in standard dirs");
        return;
    }
    println!("{} candidate(s) in standard dirs:", paths.len());
    for path in &paths {
        let Some(p) = path.to_str() else {
            eprintln!("warn: {path:?}: not valid UTF-8, skipped");
            continue;
        };
        let loader = match unsafe { Loader::open(p) } {
            Ok(l) => l,
            Err(e) => {
                eprintln!("warn: {p}: {e}");
                continue;
            }
        };
        for idx in 0..loader.plugin_count() {
            if let Some(d) = loader.descriptor(idx) {
                let name = unsafe { CStr::from_ptr(d.name) }.to_string_lossy();
                let id = unsafe { CStr::from_ptr(d.id) }.to_string_lossy();
                println!("{p}: {name}  id={id}");
            }
        }
    }
}

/// The created plugin's display name and id, for the GUI header.
///
/// # Safety
/// `plugin` must be a live plugin instance returned by `Loader::create`.
unsafe fn plugin_name_id(plugin: *const clap_plugin) -> (String, String) {
    let d = unsafe { (*plugin).desc };
    unsafe {
        (
            CStr::from_ptr((*d).name).to_string_lossy().into_owned(),
            CStr::from_ptr((*d).id).to_string_lossy().into_owned(),
        )
    }
}

/// CLI `--play`: open the output device and block until ctrl+c. The audio
/// `run()` equivalent lives here in the app (core keeps only `audio::open`).
fn run_play(
    plugin: *const clap_plugin,
    midi_rx: Queue<RawMidi>,
    ui_rx: Queue<UiEvent>,
    device_name: Option<&str>,
    input_name: Option<&str>,
) {
    let session = audio::open(plugin, device_name, input_name, midi_rx, ui_rx).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    println!(
        "audio ports: in {:?} / out {:?} (channels per port), note dialect {:?}",
        session.in_ports, session.out_ports, session.dialect
    );
    println!(
        "playing at {} Hz, {}ch — ctrl+c to stop",
        session.sample_rate, session.device_channels
    );
    loop {
        pump_main_thread(plugin);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
