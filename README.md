# CLAP-Host-RS

Standalone CLAP host in Rust: load a `.clap`/`.dll` plugin, drive it with MIDI, and output audio — via CLI or a small Slint GUI.

Status: pre-alpha. One plugin per process; no plugin graph yet.

## Build & test

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The binary is produced at `target/debug/clap-host-rs` (`--release` works too).

## CLI

```
clap-host-rs [--scan | --plugin <path.clap> [--id <clap-id>]
             [--list-params] [--list-presets]
             [--pull-preset <key> --out <file>] [--set <id>=<val>]...
             [--load-state <file>] [--save-state <file>]
             [--play | --gui [--output-device <name>] [--input-device <name>]]
             [--list-midi] [--midi-in <name>]] [--list-devices]
```

- `--scan` — walk the OS standard CLAP dirs and list found plugins.
- `--list-devices` — list audio outputs, audio inputs and MIDI inputs.
- `--plugin <path>` — open a plugin binary; plugins inside are listed.
  `--id <clap-id>` selects which one to instantiate when there are several.
- `--list-params` — show every visible parameter with value, text and range.
- `--list-presets` — list factory presets via the preset-discovery.
- `--pull-preset <key> --out <file>` — stream a factory preset to a file.
- `--set <id>=<val>` — set parameters before activation (repeatable).
- `--load-state <file>` / `--save-state <file>` — plugin state round-trip
  (load right after `init`, save after `--set`/preset pull).
- `--play` — open the audio device and process until Ctrl+C. `--output-device`/
  `--input-device` pick non-default devices; the input feeds the plugin's
  audio input ports. `--midi-in <name>` selects a MIDI input port.
- `--gui` — open the Slint window instead of blocking on the console.

`--scan`, `--list-midi` and `--list-devices` are valid standalone queries
without `--plugin`.

## GUI

Slint window (`--gui`): output/input device pickers, a scrollable parameter
list with sliders, a computer-keyboard piano (focused window = playing), a
toggle for the plugin's floating window, and state save/load with a dirty
indicator (the plugin's `state.mark_dirty` lights a dot; save/load use
`<plugin-file>.state.bin` next to the plugin binary).

## Architecture

Two crates:

- `clap-host-core` — host-shaped logic without UI toolkit: plugin loading and
  host callbacks (`loader`, `host`), the cpal audio session (`audio`,
  `start_processing`/`stop_processing` run on the audio thread), lock-free
  MIDI/UI event queues (`events`), MIDI input (`midi`), floating plugin
  window helper (`plugin_gui`), preset discovery (`preset`), standard-dir
  scanner (`scan`) and plugin state save/load (`state`).
- `clap-host-app` — the binary: simple CLI parsing, `main` flow and the Slint
  GUI (`ui/*.slint`, `gui.rs`). The app handles printing and process lifetime;
  the core remains library-clean.

## Follow-ups

Known gaps, roughly ordered by usefulness:

- Plugin window embedding (`SetParent`/`WS_CHILD`) — plugin window currently
  floats beside the host window.
- `remote-controls` host extension for DAW-like control surfaces.
- Audio thread pool — one cpal callback per plugin blocks a multi-plugin
  graph.
- Transport/timeline (`clap_transport`) — the engine currently passes a null
  transport.
- Multi-plugin graph with connections — the session owns exactly one plugin.
- `CLAP_PATH` env var support in the scanner (spec) — only standard dirs are
  scanned today.
- Symlink-cycle guard and a `"(none)"` entry in the GUI input picker.
