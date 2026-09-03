# CLAP-Host-RS

Standalone CLAP host in Rust: load a `.clap`/`.dll` plugin, drive it with MIDI, and output audio — via CLI or a small Slint GUI.

Status: pre-alpha. One plugin per process; no plugin graph yet. The
implementation plan and GUI follow-ups in `planning/` are fully implemented
(see the "Umsetzungsstand" section at the top of each plan); deferred items
are listed under Follow-ups below.

## Build & test

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The binary is produced at `target/debug/clap-host-rs` (`--release` works too).

### Optional: ASIO backend (Windows)

The default Windows backend is WASAPI. ASIO support is behind the opt-in
feature `asio` (cpal's ASIO host):

```bash
cargo build --workspace --features asio
```

Building it needs more than a normal build: the Steinberg ASIO SDK, pointed
to by the `CPAL_ASIO_DIR` env var, plus LLVM/Clang for bindgen
(`LIBCLANG_PATH`). See [cpal's README](https://github.com/RustAudio/cpal)
("ASIO on Windows"). Without the SDK the default (WASAPI) build is
unaffected.

## CLI

```
clap-host-rs [--scan | --plugin <path.clap> [--id <clap-id>]
             [--list-params] [--list-presets]
             [--pull-preset <key> --out <file>] [--set <id>=<val>]...
             [--load-state <file>] [--save-state <file>]
             [--play | --gui [--output-device <name>] [--input-device <name>]
             [--sample-rate <hz>] [--buffer-size <frames>]]
             [--list-midi] [--midi-in <name>]] [--list-devices]
```

- `--scan` — walk the OS standard CLAP dirs and list found plugins. The
  `CLAP_PATH` env var (`;`-separated on Windows, `:` elsewhere) adds extra
  search dirs, per the CLAP spec.
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
  `--sample-rate <hz>` and `--buffer-size <frames>` override the
  device/backend defaults (the buffer-size falls back to the backend default
  if the backend rejects a fixed size).
- `--gui` — open the Slint window instead of blocking on the console.

`--scan`, `--list-midi` and `--list-devices` are valid standalone queries
without `--plugin`.

## GUI

Slint window (`--gui`): a Setup dialog for audio/MIDI device selection —
output and audio-input devices, sample rate, buffer size, MIDI port, applied
immediately on every change — plus a filterable, scrollable parameter list
with sliders, a computer-keyboard piano (focused window = playing), a toggle
for the plugin's own window, and state save/load with a dirty indicator (the
plugin's `state.mark_dirty` lights a dot; save/load use `<plugin-file>.state.bin`
next to the plugin binary).
Plugins exposing `clap.remote-controls` get a paging panel (page name +
prev/next buttons) whose sliders mirror the plugin's parameter pages.

Plugin GUI toggle behavior is Reaper-style: opening the editor swaps the
parameter page for the plugin's own UI — only the plugin window plus the
host's top bar (save/load/setup buttons, status) stays visible. If the
plugin can't float, its editor is embedded into the host window (Windows);
the editor is capped to the monitor's work area if the plugin asks for more,
and the host window's previous size is restored when the editor closes.

## Architecture

Two crates:

- `clap-host-core` — host-shaped logic without UI toolkit: plugin loading and
  host callbacks (`loader`, `host`), the cpal audio session (`audio`,
  `start_processing`/`stop_processing` run on the audio thread), lock-free
  MIDI/UI event queues (`events`), MIDI input (`midi`), floating plugin
  window helper (`plugin_gui`), preset discovery (`preset`), remote-controls
  paging helper (`remote_controls`), Win32 editor embedding (`win32_embed`),
  standard-dir scanner (`scan`) and plugin state save/load (`state`).
- `clap-host-app` — the binary: simple CLI parsing, `main` flow and the Slint
  GUI (`ui/*.slint`, `gui.rs`). The app handles printing and process lifetime;
  the core remains library-clean.

## Follow-ups

Known gaps, roughly ordered by usefulness:

- Audio thread pool — one cpal callback per plugin blocks a multi-plugin
  graph.
- Transport/timeline (`clap_transport`) — the engine currently passes a null
  transport.
- Multi-plugin graph with connections — the session owns exactly one plugin.
- Symlink-cycle guard in the scanner (Windows junctions can loop).
- Preset browser in the GUI (presets are currently CLI-only via
  `--list-presets`/`--pull-preset`).
- Live `request_resize` handling for the embedded editor.
- Editor embedding on macOS/Linux (currently Windows-only).
- Channel routing: pick a channel pair on many-channel interfaces instead of
  always using the first channels.
