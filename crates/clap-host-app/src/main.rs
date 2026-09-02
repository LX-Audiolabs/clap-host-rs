//! clap-host-rs — minimal CLAP host.
//!
//! Phase 1 is the CLI (params, presets, MIDI, audio).
//!
//! Usage:
//!   clap-host-rs --plugin <path.clap> [--id <clap-id>] [--list-params]
//!                [--list-presets] [--pull-preset <key> --out <file>]
//!                [--set <id>=<val>]... [--play [--output-device <name>]]
//!                [--list-midi] [--midi-in <name>]

#![allow(clippy::missing_safety_doc)]

mod cli;

use std::ffi::CStr;

use clap_host_core::audio;
use clap_host_core::events::{self, Queue, RawMidi, UiEvent};
use clap_host_core::host::{make_host, pump_main_thread};
use clap_host_core::loader::{self, Loader};
use clap_host_core::midi;
use clap_host_core::preset;

use clap_host_core::clap_sys::plugin::clap_plugin;

fn main() {
    let args = cli::parse_args();

    if args.list_midi {
        print_midi_ports();
    }

    let Some(path) = args.plugin_path.clone() else {
        // --list-midi alone is a valid standalone query.
        let standalone_midi = args.list_midi
            && !args.play
            && !args.list_params
            && !args.list_presets
            && args.pull_preset.is_none()
            && args.sets.is_empty();
        if !standalone_midi {
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
        || !args.sets.is_empty()
        || args.pull_preset.is_some();
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

    if args.list_params {
        print_params(plugin);
    }

    if args.play {
        let midi_q = events::queue();
        // Connection must outlive the stream: dropping it closes the MIDI port.
        let _conn = midi::open(args.midi_in.as_deref(), &midi_q)
            .inspect_err(|e| eprintln!("warn: {e} — running without MIDI"))
            .ok();
        run_play(plugin, midi_q, events::queue(), args.output_device.as_deref());
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

/// CLI `--play`: open the output device and block until ctrl+c. The audio
/// `run()` equivalent lives here in the app (core keeps only `audio::open`).
fn run_play(
    plugin: *const clap_plugin,
    midi_rx: Queue<RawMidi>,
    ui_rx: Queue<UiEvent>,
    device_name: Option<&str>,
) {
    let session = audio::open(plugin, device_name, midi_rx, ui_rx).unwrap_or_else(|e| {
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
