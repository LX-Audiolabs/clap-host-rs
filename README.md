# CLAP-Host-RS

Standalone CLAP host (audio plugin host) in Rust: load a `.clap`/`.dll` plugin,
drive it with MIDI, audio it out — from the CLI or a small Slint GUI.

Status: pre-alpha. Single plugin per process; no plugin graph yet.

## Build & test

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The binary lands in `target/debug/clap-host-rs` (`--release` works too).

## CLI

```
clap-host-rs [--scan | --plugin <path.clap> [--id <clap-id>]
             [--list-params] [--list-presets]
             [--pull-preset <key> --out <file>] [--set <id>=<val>]...
             [--load-state <file>] [--save-state <file>]
             [--play | --gui [--output-device <name>] [--input-device <name>]]
             [--list-midi] [--midi-in <name>]] [--list-devices]
```

- `--scan` — walk the OS standard CLAP dirs and list every plugin found.
- `--list-devices` — audio output, audio input and MIDI input devices.
- `--plugin <path>` — open a plugin binary; the plugins inside are always listed.
  `--id <clap-id>` selects which one to instantiate when there are several.
- `--list-params` — every visible parameter with value, text and range.
- `--list-presets` — factory presets via the preset-discovery factory.
- `--pull-preset <key> --out <file>` — stream a factory preset into a file.
- `--set <id>=<val>` — set parameters before activation (repeatable).
- `--load-state <file>` / `--save-state <file>` — plugin state round-trip
  (load right after `init`, save after `--set`/preset pull).
- `--play` — open the audio device, process until ctrl+c. `--output-device` /
  `--input-device` pick non-default devices; the input feeds the plugin's
  audio input ports. `--midi-in <name>` selects a MIDI input port.
- `--gui` — open the Slint window instead of blocking on the console.

`--scan`, `--list-midi` and `--list-devices` are valid standalone queries
without `--plugin`.

## GUI

Slint window (`--gui`): output/input device pickers, a scrollable parameter
list with sliders, a computer-keyboard piano (focused window = playing),
a toggle for the plugin's own floating window, and state save/load with a
dirty indicator (the plugin's `state.mark_dirty` lights a dot; save/load use
`<plugin-file>.state.bin` next to the plugin binary).

## Architecture

Two crates:

- `clap-host-core` — everything host-shaped, no UI toolkit: plugin loading
  and host callbacks (`loader`, `host`), the cpal audio session (`audio`,
  `start_processing`/`stop_processing` run on the audio thread), lock-free
  MIDI/UI event queues (`events`), MIDI input (`midi`), the floating plugin
  window helper (`plugin_gui`), preset discovery (`preset`), the standard-dir
  scanner (`scan`) and plugin state save/load (`state`).
- `clap-host-app` — the binary: hand-rolled CLI parser, `main` flow, and the
  Slint GUI (`ui/*.slint`, `gui.rs`). The app owns all printing and
  process-lifetime decisions; the core stays library-clean.

## Follow-ups

Known gaps, roughly in order of usefulness:

- Plugin window embedding (`SetParent`/`WS_CHILD`) — currently the plugin
  window floats beside the host window.
- `remote-controls` host extension for DAW-like control surfaces.
- Audio thread pool — one cpal callback per plugin blocks a multi-plugin graph.
- Transport/timeline (`clap_transport`) — the engine passes a null transport.
- Multi-plugin graph with connections — the session owns exactly one plugin.
- `CLAP_PATH` env var in the scanner (spec) — only standard dirs are scanned.
- Symlink-cycle guard and a `"(none)"` entry in the GUI input picker.
