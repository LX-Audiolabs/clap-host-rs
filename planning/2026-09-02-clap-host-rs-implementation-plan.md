# CLAP-Host-RS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Standalone Rust CLAP-Host (Plugin-Dev-Host) als Workspace aus `clap-host-core` (Lib, kein Slint) + `clap-host-app` (CLI + Slint GUI), kopiert aus aura-host und erweitert um Scanner, Audio-IN-Auswahl und State save/load.

**Architecture:** Kopier-Port aus `L:\LX-Audiolabs\AURA\crates\aura-host` (READ-ONLY — niemals ändern) in ein neues Workspace. Core übernimmt Loader/Host-Callbacks, Events, Audio-Engine, MIDI, Presets, Floating-GUI-Helpers, Scanner, State. App übernimmt CLI-Parser, Slint-Shell, Device-Picker, Druck-Helfer. Wachstum in 6 Phasen: Scaffold → Core-Port → Scanner/Audio-IN → Slint-App → Host-Extensions/State → Hardening.

**Tech Stack:** Rust 1.92 / edition 2024, clap-sys 0.5, libloading 0.9, cpal 0.18.2, midir 0.11, crossbeam-queue 0.3, slint =1.17.1 (backend-winit, renderer-femtovg, compat-1-2), slint-build =1.17.1.

**Spec:** `planning/2026-09-02-clap-host-rs-design.md` (Pflichtlektüre, Abschnitt "Port mapping from aura-host")

## Roadmap

| Phase | Tasks | Deliverable |
|-------|-------|-------------|
| 0 Scaffold | T1 | Workspace baut, git init, MIT |
| 1 Core-Port | T2–T6 | CLI-Host ohne GUI: laden, params, presets, MIDI, `--play` |
| 2 Scanner + Audio-IN | T7–T8 | `--scan`, `--list-devices`, wählbares Input-Device |
| 3 Slint-App | T9–T12 | GUI mit Device-Pickern (out/in/MIDI), Params, Keyboard-Notes, floating Plugin-GUI |
| 4 Extensions + State | T13–T15 | Host-Extensions (params/state/timer/latency), State save/load in CLI + UI |
| 5 Hardening | T16 | clippy clean, Tests grün, README + Smoke-Checkliste |

## Global Constraints

- Quelle `L:\LX-Audiolabs\AURA\crates\aura-host` und `L:\LX-Audiolabs\AURA\crates\aura-build\ui\` sind **read-only**.
- Core (`clap-host-core`) hat **keine** Slint-Abhängigkeit; GUI-Code kommt nur in `clap-host-app`.
- Rebrand: Host-Name `"CLAP-Host-RS"`, Vendor `"lxndrbe"`, URL `https://github.com/lxndrbe/clap-host-rs`; MIDI-Client-Namen `clap-host-rs` / `clap-host-rs-list`.
- Kein `windows-sys`, kein `raw-window-handle`, kein `win32_embed.rs` (V1 floating only).
- CLI-Helfer mit `println!`/`process::exit` gehören in die App, nicht in den Core.
- Lizenz MIT (nicht AURAs GPL). `publish = false`. Version `0.1.0`.
- slint exakt `=1.17.1` pinnen (app + build-dep).
- Nach jeder Task: `cargo build --workspace` grün; `cargo test --workspace` grün wo Tests existieren; erst dann commit.
- Windows/Git-Bash-Umgebung; Pfade in Cargo/Slint-Imports immer forward-slash.

---

### Task 1: Workspace-Scaffold

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `LICENSE`, `README.md`, `crates/clap-host-core/Cargo.toml`, `crates/clap-host-core/src/lib.rs`, `crates/clap-host-app/Cargo.toml`, `crates/clap-host-app/src/main.rs`

**Interfaces:**
- Produces: Workspace `clap-host-rs` mit Packages `clap-host-core` (lib) und `clap-host-app` (bin `clap-host-rs`); `[workspace.lints.clippy] pedantic = "warn"`; profiles `panic = "unwind"`.

- [ ] **Step 1: git init**

```bash
cd /l/CLAP-Host-RS && git init && git add planning/ && git commit -m "docs: design spec and implementation plan"
```

- [ ] **Step 2: Workspace `Cargo.toml` schreiben**

```toml
[workspace]
resolver = "3"
members = ["crates/clap-host-core", "crates/clap-host-app"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.92"
license = "MIT"
repository = "https://github.com/lxndrbe/clap-host-rs"
authors = ["lxndrbe"]

[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }

[profile.dev]
panic = "unwind"

[profile.release]
panic = "unwind"
```

- [ ] **Step 3: `crates/clap-host-core/Cargo.toml`**

```toml
[package]
name = "clap-host-core"
description = "Standalone CLAP host core: loading, audio, MIDI, presets, state, scan"
publish = false

[dependencies]
clap-sys = "0.5"
libloading = "0.9"
cpal = "0.18.2"
midir = "0.11"
crossbeam-queue = "0.3"
```

- [ ] **Step 4: `crates/clap-host-app/Cargo.toml`**

```toml
[package]
name = "clap-host-app"
publish = false

[[bin]]
name = "clap-host-rs"
path = "src/main.rs"

[dependencies]
clap-host-core = { path = "../clap-host-core" }

[build-dependencies]
slint-build = "=1.17.1"
```

(slint-Dep kommt in Task 9 dazu — bewusst weggelassen, YAGNI.)

- [ ] **Step 5: Minimal `lib.rs` / `main.rs`, `.gitignore`, LICENSE, README**

`crates/clap-host-core/src/lib.rs`:
```rust
//! CLAP host core — plugin loading, audio session, MIDI, presets, state, scan.
```

`crates/clap-host-app/src/main.rs`:
```rust
fn main() {
    println!("clap-host-rs");
}
```

`.gitignore`:
```
/target
.DS_Store
```

LICENSE: MIT-Standardtext, Copyright (c) 2026 lxndrbe. README.md: Titel, ein Satz Zweck, Build (`cargo build --workspace`), Status "pre-alpha".

- [ ] **Step 6: Verifizieren + commit**

```bash
cargo build --workspace && cargo clippy --workspace --all-targets
```
Erwartet: baut ohne Fehler (pedantic-Warnungen ok). Dann `git add -A && git commit -m "chore: workspace scaffold"`.

---

### Task 2: events.rs portieren (Core)

**Files:**
- Create: `crates/clap-host-core/src/events.rs`
- Test: enthalten (aura events.rs:196-234 wird mitkopiert)

**Interfaces:**
- Produces: `clap_host_core::events` mit `RawMidi = [u8; 3]`, `QUEUE_CAP`, `UiEvent::{Param, Midi}`, `Queue<T> = Arc<ArrayQueue<T>>`, `queue()`, `Dialect::{Midi, Clap, None}`, `EvList::{with_capacity, clear, push_param, push_midi, as_input_events}`, `sink_output_events()`.

- [ ] **Step 1: Datei kopieren**

Kopiere `L:\LX-Audiolabs\AURA\crates\aura-host\src\events.rs` 1:1 nach `crates/clap-host-core/src/events.rs`. Keine inhaltlichen Änderungen nötig (keine `crate::`-Imports, keine Aura-Strings).

- [ ] **Step 2: Modul registrieren**

In `lib.rs`:
```rust
pub mod events;
```

- [ ] **Step 3: Tests laufen lassen**

```bash
cargo test -p clap-host-core
```
Erwartet: `event_layout_and_dialect` (und ggf. weitere mitkopierte Tests) grün.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(core): port events module from aura-host"
```

---

### Task 3: host.rs + loader.rs (Core)

**Files:**
- Create: `crates/clap-host-core/src/host.rs` (Zeilen 39–203 aus aura loader.rs)
- Create: `crates/clap-host-core/src/loader.rs` (Rest aus aura loader.rs, angepasst)
- Modify: `crates/clap-host-core/src/lib.rs`

**Interfaces:**
- Consumes: `events::{sink_output_events, Dialect}` (Task 2).
- Produces: `clap_host_core::host` mit `make_host() -> &'static clap_host`, `mark_audio_thread()`, `pump_main_thread(plugin)`, `take_restart_request() -> bool`, `take_gui_closed() -> bool`. `clap_host_core::loader` mit `Loader::{open, get_factory, plugin_count, descriptor, create}`, `plugin_ext()`, `audio_port_channels()`, `note_dialect()`, `set_param()`, `ParamInfo`, `params()`, `param_value()`, `param_text()`, `PluginPtr`. **Kein** `list_params` (println-Version → App, Task 6).

- [ ] **Step 1: host.rs extrahieren**

Neue Datei `host.rs`: übernehme aus aura `loader.rs` die Zeilen 39–203 (Statics `MAIN_THREAD`/`AUDIO_THREAD`, `mark_audio_thread`, Flag-Statics + `pump_main_thread`/`take_restart_request`/`take_gui_closed`, Extension-Statics `LOG_EXT`/`GUI_EXT`/`PARAMS_EXT`/`THREAD_CHECK_EXT`, `host_get_extension`, `make_host`). Anpassungen:
- `use crate::events::sink_output_events;` und `use crate::events::Dialect;` (statt `crate::events::{Dialect, EvList, sink_output_events}` — `EvList` wird hier nicht gebraucht; exakten Bedarf aus dem Original prüfen).
- Identity-Strings (Original loader.rs:186-188): name `"CLAP-Host-RS"`, vendor `"lxndrbe"`, url `https://github.com/lxndrbe/clap-host-rs`.
- Alles `pub` machen, was loader/audio später brauchen (`make_host`, Pumpen/Take-Fns, `mark_audio_thread`).

- [ ] **Step 2: loader.rs portieren**

Neue Datei `loader.rs`: kopiere aura `loader.rs` vollständig, entferne die nach host.rs ausgelagerten Zeilen 39–203 (Host-Statics, Extension-Statics, `host_get_extension`, `make_host`), passe Imports an:
```rust
use crate::events::{Dialect, EvList, sink_output_events};
use crate::host::mark_audio_thread; // falls direkt referenziert, sonst weglassen
```
Lösche `list_params` (Original loader.rs:471-...; println-Version) — die App druckt selbst über `params()`.

- [ ] **Step 3: lib.rs**

```rust
pub mod events;
pub mod host;
pub mod loader;
```

- [ ] **Step 4: Build + clippy**

```bash
cargo build -p clap-host-core && cargo clippy -p clap-host-core --all-targets
```
Erwartet: baut; Warnungen (z.B. pedantic um `unsafe`) notieren, in Task 16 adressieren.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): port loader and split host callbacks into host.rs"
```

---

### Task 4: midi.rs + preset.rs (Core)

**Files:**
- Create: `crates/clap-host-core/src/midi.rs`, `crates/clap-host-core/src/preset.rs`
- Modify: `crates/clap-host-core/src/lib.rs`

**Interfaces:**
- Consumes: `events::{Queue, RawMidi}`, `loader::{Loader, plugin_ext}`.
- Produces: `midi::{port_names(), open(want, queue) -> Result<MidiInputConnection<()>, String>}` (**kein** `list_ports` — println → App). `preset::{ListedPreset, list(&Loader) -> Result<Vec<ListedPreset>, String>, pull(plugin, key, out: &Path)}` (**kein** `print_list` — println + process::exit → App).

- [ ] **Step 1: midi.rs kopieren + rebranden**

Kopiere aura `midi.rs` (78 LOC). Änderungen:
- Client-Namen: `"aura-host-list"` → `"clap-host-rs-list"` (Zeile 16), `"aura-host"` → `"clap-host-rs"` (Zeile 37).
- `list_ports()` (Zeile 25) streichen — die App iteriert selbst über `port_names()`.

- [ ] **Step 2: preset.rs kopieren + rebranden**

Kopiere aura `preset.rs` (264 LOC). Änderungen:
- Indexer-Name/Vendor (Zeilen 100-101): `"clap-host-rs"` / `"lxndrbe"`.
- `print_list()` (Zeile 179) streichen — App ruft `list()` auf und druckt selbst.

- [ ] **Step 3: lib.rs um `pub mod midi; pub mod preset;` erweitern.**

- [ ] **Step 4: Build + commit**

```bash
cargo build -p clap-host-core && cargo clippy -p clap-host-core --all-targets
git add -A && git commit -m "feat(core): port midi and preset modules"
```

---

### Task 5: audio.rs portieren (Core)

**Files:**
- Create: `crates/clap-host-core/src/audio.rs`
- Modify: `crates/clap-host-core/src/lib.rs`

**Interfaces:**
- Consumes: `events::{Dialect, EvList, Queue, RawMidi, UiEvent, sink_output_events}`, `loader::{self, PluginPtr}` (`audio_port_channels`, `note_dialect`, `mark_audio_thread`).
- Produces: `audio::{MAX_FRAMES, Engine, Session, output_devices(), open(plugin, device_name, midi_rx, ui_rx) -> Result<Session, String>}`. **Kein** `run()` (CLI-Blocker → App, Task 6). Signatur von `open` in Task 8 erweitert — hier 1:1-Port.

- [ ] **Step 1: Datei kopieren + anpassen**

Kopiere aura `audio.rs` (416 LOC). Änderungen:
- Imports: `use crate::events::...;` und `use crate::loader::{self, PluginPtr};` (Pfade passen bereits zu `crate::`, da gleiche Struktur).
- `run()` (Zeilen 400-416) streichen.
- Ansonsten 1:1 — Input-Device-Wahl kommt in Task 8.

- [ ] **Step 2: lib.rs um `pub mod audio;` erweitern.**

- [ ] **Step 3: Build + commit**

```bash
cargo build -p clap-host-core && cargo clippy -p clap-host-core --all-targets
git add -A && git commit -m "feat(core): port audio engine and session"
```

---

### Task 6: CLI-Binary (App, Phase-1-Abschluss)

**Files:**
- Create: `crates/clap-host-app/src/main.rs` (ersetzt Hello-World), `crates/clap-host-app/src/cli.rs`
- Modify: `crates/clap-host-app/Cargo.toml`

**Interfaces:**
- Consumes: gesamter Core (`loader`, `host`, `audio`, `midi`, `preset`, `events`).
- Produces: CLI `clap-host-rs` mit Flags: `--plugin <path> [--id <clap-id>]`, `--play [--output-device <name>]`, `--list-params`, `--list-presets`, `--pull-preset <key> --out <path>`, `--set <id=value>...`, `--list-midi`, `--midi-in <name>`. Druck-Helfer (`print_params`, `print_presets`, `print_midi_ports`, `run_play`-Schleife mit ctrl+c) leben in der App.

- [ ] **Step 1: cli.rs schreiben**

Portiere Aura's handgerollten Parser (`main.rs:34-115`, `Args`/`parse_args`/`parse_set`) nach `cli.rs`, Flags wie oben. Keine CLI-Library (Parität mit aura-host, YAGNI).

- [ ] **Step 2: main.rs schreiben**

Portiere Aura's `main()`-Ablauf (main.rs:137-231) als `clap-host-rs`-Binary: Loader öffnen → optional `--list-presets` → Instanz via `host::make_host()` + `Loader::create` → `init` → `--set`-Flush via `loader::set_param` → optional `--pull-preset` → optional `--list-params` → `audio::run`-Äquivalent (hier in App: `run_play(plugin, midi_rx, ui_rx, device_name)` — Schleife `loop { pump_main_thread; sleep }` bis ctrl+c, siehe aura audio.rs:400-416) → `destroy`. Fehler: `eprintln!` + Exit-Code 1 statt `process::exit` mitten im Core.

- [ ] **Step 3: Cargo.toml der App — core-Dep prüfen**

`clap-host-core = { path = "../clap-host-core" }` steht bereits (Task 1). Sonst nichts ändern.

- [ ] **Step 4: Manuelle Smoke-Tests**

```bash
cargo build --workspace
# Liste der Plugins in einer .clap:
cargo run -p clap-host-app -- --plugin /pfad/zu/plugin.clap --list-params
# Presets:
cargo run -p clap-host-app -- --plugin /pfad/zu/plugin.clap --list-presets
# Audio (manuell abbrechen):
cargo run -p clap-host-app -- --plugin /pfad/zu/plugin.clap --play
```
Erwartet: Ausgaben wie im aura-host; Audio läuft, bis ctrl+c.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(app): CLI host with params, presets, midi, play"
```

---

### Task 7: Plugin-Scanner `scan.rs` + `--scan`

**Files:**
- Create: `crates/clap-host-core/src/scan.rs`
- Test: `crates/clap-host-core/src/scan.rs` (`#[cfg(test)]`)
- Modify: `crates/clap-host-core/src/lib.rs`, `crates/clap-host-app/src/cli.rs`, `crates/clap-host-app/src/main.rs`

**Interfaces:**
- Produces: `scan::{standard_dirs() -> Vec<PathBuf>`, `scan_dir(&Path) -> Vec<PathBuf>`, `scan() -> Vec<PathBuf>}` — liefert Pfade zu `*.clap`-Dateien (inkl. Symlink-Dedup optional, YAGNI: simple Vec, sortiert + dedup).
- Consumes: nichts (reine Pfadlogik).

- [ ] **Step 1: Failing tests schreiben**

In `scan.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_dir_finds_clap_files_recursively() {
        let root = std::env::temp_dir().join("clap-host-rs-scan-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.clap"), b"").unwrap();
        std::fs::write(root.join("sub/b.clap"), b"").unwrap();
        std::fs::write(root.join("ignore.txt"), b"").unwrap();
        let mut found = scan_dir(&root);
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("a.clap"));
        assert!(found[1].ends_with("sub/b.clap"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn standard_dirs_has_entries_on_all_platforms() {
        assert!(!standard_dirs().is_empty());
    }
}
```

- [ ] **Step 2: Tests laufen — erwarten FAIL**

```bash
cargo test -p clap-host-core scan
```
Erwartet: Compile-Fehler (`scan_dir`/`standard_dirs` unbekannt).

- [ ] **Step 3: Implementieren**

```rust
//! CLAP plugin scanner: OS standard search paths.

use std::path::{Path, PathBuf};

/// OS-standard CLAP search directories (per CLAP spec).
pub fn standard_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(p) = std::env::var("COMMONPROGRAMFILES") {
            dirs.push(PathBuf::from(p).join("CLAP"));
        }
        if let Ok(p) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(p).join("Programs").join("Common").join("CLAP"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Ok(h) = std::env::var("HOME") {
            dirs.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/CLAP"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/lib/clap"));
        dirs.push(PathBuf::from("/usr/local/lib/clap"));
        if let Ok(h) = std::env::var("HOME") {
            dirs.push(PathBuf::from(h).join(".clap"));
        }
    }
    dirs
}

/// Recursively collect `*.clap` files under `dir` (missing dir → empty).
pub fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(scan_dir(&path));
        } else if path.extension().is_some_and(|e| e == "clap") {
            out.push(path);
        }
    }
    out
}

/// All `*.clap` plugins in all standard dirs, sorted + deduped.
pub fn scan() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = standard_dirs().iter().flat_map(|d| scan_dir(d)).collect();
    out.sort();
    out.dedup();
    out
}
```

Hinweis: Pfadliste gegen https://github.com/free-audio/clap (README, Plugin Discovery) verifizieren; Abweichungen korrigieren.

- [ ] **Step 4: Tests grün**

```bash
cargo test -p clap-host-core scan
```

- [ ] **Step 5: CLI-Flag `--scan`**

In `cli.rs` Flag `scan: bool` (kein `--plugin` nötig); in `main.rs`: bei `--scan` jeden gefundenen Pfad mit `Loader::open` + Plugin-Deskriptor-Namen ausgeben, Fehler pro Datei als Warnung überspringen (ein kaputter Plugin darf den Scan nicht stoppen).

- [ ] **Step 6: Smoke + commit**

```bash
cargo run -p clap-host-app -- --scan
git add -A && git commit -m "feat(core,app): plugin scanner with standard paths and --scan"
```

---

### Task 8: Audio-IN Gerätewahl + `--list-devices`

**Files:**
- Modify: `crates/clap-host-core/src/audio.rs` (`open`, `open_input`, neu `input_devices()`)
- Modify: `crates/clap-host-app/src/cli.rs`, `crates/clap-host-app/src/main.rs`

**Interfaces:**
- Consumes: aura audio.rs Stand (Task 5).
- Produces: `audio::{input_devices() -> Vec<String>`, `open(plugin, device_name: Option<&str>, input_name: Option<&str>, midi_rx, ui_rx) -> Result<Session, String>}`. `input_name = None` ⇒ bisheriges Verhalten (`default_input_device()`).

- [ ] **Step 1: `input_devices()` hinzufügen**

Analog zu `output_devices()` (aura audio.rs:224): `cpal::default_host().input_devices()`, Label via bestehendem `device_label()`.

- [ ] **Step 2: `open_input` parametrisieren**

Aura audio.rs:356-363: Signatur zu `fn open_input(rate: cpal::SampleRate, plugin_in_ch: usize, want: Option<&str>)`. Statt `default_input_device()`:
```rust
let dev = match want {
    Some(name) => cpal::default_host()
        .input_devices()
        .map_err(|e| e.to_string())?
        .find(|d| device_label(d) == name),
    None => cpal::default_host().default_input_device(),
};
let Some(device) = dev else {
    return Err(format!("input device '{want:?}' not found"));
};
```
Aufrufstelle in `open` (aura audio.rs:294-297) mitreichen. `Session::open`/`open` bekommt neuen Parameter `input_name: Option<&str>` **vor** `midi_rx`.

- [ ] **Step 3: App anpassen**

`cli.rs`: Flags `--list-devices`, `--input-device <name>` (nur wirksam mit `--play`). `main.rs`: `--list-devices` druckt Output-, Input- und MIDI-Ports (`audio::output_devices()`, `audio::input_devices()`, `midi::port_names()`); `run_play` reicht `input_name` an `audio::open` durch.

- [ ] **Step 4: Smoke + commit**

```bash
cargo build --workspace
cargo run -p clap-host-app -- --list-devices
cargo run -p clap-host-app -- --plugin /pfad/zu/instrument.clap --play --input-device "<Name aus Liste>"
```
Erwartet: Input-Device wird geöffnet (keine "using default"-Warnung für gewähltes Gerät); Instrument bekommt Capture-Signal. Dann:
```bash
git add -A && git commit -m "feat(core,app): selectable audio input device, --list-devices"
```

---

### Task 9: Slint-UI-Foundation (Theme + ParamSlider, ohne @aura)

**Files:**
- Create: `crates/clap-host-app/build.rs`, `crates/clap-host-app/ui/theme.slint`, `crates/clap-host-app/ui/widgets.slint`, `crates/clap-host-app/ui/host.slint`
- Modify: `crates/clap-host-app/Cargo.toml`, `crates/clap-host-app/src/main.rs`

**Interfaces:**
- Produces: `ui/host.slint` exportiert `HostWindow` (Properties/Callbacks identisch zu aura host.slint), importiert ersetzt: `import { HostTheme, ParamSlider } from "widgets.slint"` — Widget-API bleibt: `ParamSlider { label, value, minimum, maximum, text; changed(float) }` (aura host.slint nutzt `param-changed`-Callback via Zeilen 178-190; API 1:1 übernehmen).

- [ ] **Step 1: Vorlagen lesen**

Lies `L:\LX-Audiolabs\AURA\crates\aura-build\ui\theme.slint` und `slider.slint` (Vorlagen für Theme-Tokens und ParamSlider).

- [ ] **Step 2: `theme.slint` + `widgets.slint` schreiben**

`theme.slint`: global `HostTheme` mit den Farben/Font-Größen aus AuraTheme (Werte 1:1 übernehmen, Namen `host-*`/`HostTheme`). `widgets.slint`: `ParamSlider` aus aura slider.slint, Import auf `theme.slint` umschreiben, `AuraTheme`-Referenzen → `HostTheme`.

- [ ] **Step 3: `host.slint` portieren**

Kopiere aura `ui/host.slint` (219 LOC), ersetze Zeile 4 (`import { AuraTheme, ParamSlider } from "@aura"`) durch `import { HostTheme } from "theme.slint"; import { ParamSlider } from "widgets.slint";`, `AuraTheme` → `HostTheme` im Body, Fenstertitel `"aura-host"` → `"CLAP-Host-RS"`. Rest 1:1 (inkl. FocusScope-Keyboard, Zeilen 53-78).

- [ ] **Step 4: build.rs + Cargo.toml**

`build.rs`:
```rust
fn main() {
    slint_build::compile("ui/host.slint").unwrap();
}
```
App `Cargo.toml`: `[dependencies] slint = { version = "=1.17.1", default-features = true, features = ["backend-winit", "renderer-femtovg", "compat-1-2"] }`.

- [ ] **Step 5: Kompilier-Smoke**

`main.rs` temporär `slint::include_modules!();` + `fn main() { let _ = HostWindow::new().unwrap(); }` — nur bauen, nicht committed als Dauerzustand:
```bash
cargo build -p clap-host-app
```
Erwartet: UI kompiliert (slint-build läuft durch). Dann Step 6.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(app): slint UI foundation with own theme and ParamSlider"
```

---

### Task 10: gui.rs portieren (floating only) + `--gui`

**Files:**
- Create: `crates/clap-host-app/src/gui.rs`
- Modify: `crates/clap-host-app/src/main.rs` (mod gui + `--gui`-Pfad)

**Interfaces:**
- Consumes: Core (`loader::{params, param_value, param_text, ParamInfo, pump_main_thread, take_gui_closed, take_restart_request}`, `audio::{self, Session}`, `events::{self, Queue}`, `midi`), `slint::include_modules!` (Task 9).
- Produces: `gui::run(plugin: *const clap_plugin, name: &str, id: &str, midi_in: Option<&str>, input_name: Option<&str>) -> Result<(), Box<dyn std::error::Error>>`.

- [ ] **Step 1: gui.rs portieren**

Kopiere aura `gui.rs` (347 LOC). Streiche alles `#[cfg(windows)]`-Embed-Zeug:
- `use crate::win32_embed;` (Zeile 27), `PluginWindow::Embedded`-Variante (Zeilen 37-40) → `struct` einfach `gui: Option<FloatingGui>` (kein Enum mehr), `parent_hwnd()` (Zeilen 44-53), `EMBED_X/Y` (Zeilen 60-63), Embedded-Zweig in `on_toggle_gui` (Zeilen 210-230 auf Floating reduzieren), Mindestgrößen-Guard im Timer (Zeilen 264-283).
- `PluginWindow::Floating`-Aufrufe → direkt `FloatingGui`.
- Kommentare mit AURA-Bezug verallgemeinern.
- Rest 1:1: Host-Struct, `param_rows()`, 50-ms-Timer (`pump_main_thread`, `take_gui_closed`, `take_restart_request` → Audio-Restart), Param-Polling 20 Hz, `start_audio`/`start_midi`, `push_note`.

- [ ] **Step 2: main.rs — `--gui`**

`--gui` ruft `gui::run(...)` statt `run_play(...)`; MIDI-/Input-Namen durchreichen.

- [ ] **Step 3: Smoke + commit**

```bash
cargo build --workspace
cargo run -p clap-host-app -- --plugin /pfad/zu/plugin.clap --gui
```
Erwartet: Host-Fenster öffnet, Geräte-ComboBoxen befüllt, Params sichtbar, Keyboard-Notes spielen (Output-Device beachten). Dann:
```bash
git add -A && git commit -m "feat(app): port slint gui shell, floating only"
```

---

### Task 11: plugin_gui.rs in Core + GUI-Toggle

**Files:**
- Create: `crates/clap-host-core/src/plugin_gui.rs` (Kopie aus aura)
- Modify: `crates/clap-host-core/src/lib.rs`, `crates/clap-host-app/src/gui.rs` (Import `clap_host_core::plugin_gui` statt `crate::plugin_gui`)

**Interfaces:**
- Produces: `clap_host_core::plugin_gui::{supports_floating(plugin) -> bool, FloatingGui::open(plugin, title) -> Result<FloatingGui, String>}` (+ `Drop`).
- Consumes: `loader::plugin_ext` — FloatingGui ist Slint-frei, gehört in den Core (App ruft ihn nur auf).

- [ ] **Step 1: Kopieren + registrieren**

aura `plugin_gui.rs` (80 LOC) 1:1 kopieren (`use crate::loader::plugin_ext` passt). `lib.rs`: `pub mod plugin_gui;`.

- [ ] **Step 2: App-Import umhängen**

In `gui.rs`: `use clap_host_core::plugin_gui::{supports_floating, FloatingGui};` statt `crate::plugin_gui::...`.

- [ ] **Step 3: Smoke + commit**

`--gui` → "Open plugin GUI" öffnet floating Fenster; Schließen des Plugin-Fensters setzt Button-Status zurück (`take_gui_closed`-Polling funktioniert).

```bash
cargo build --workspace && git add -A && git commit -m "feat(core): move floating plugin gui helper into core"
```

---

### Task 12: Audio-Input-Picker in der UI

**Files:**
- Modify: `crates/clap-host-app/ui/host.slint`, `crates/clap-host-app/src/gui.rs`

**Interfaces:**
- Consumes: `audio::{input_devices, output_devices}`, Session-Restart-Logik aus Task 10.
- Produces: `HostWindow` neue Properties `audio-in-devices: [string]`, `in-out audio-in-index: int`, Callback `audio-in-changed(int)`; Panel nur sichtbar wenn Plugin Input-Ports hat.

- [ ] **Step 1: host.slint erweitern**

Nach dem Output-Device-Panel (aura host.slint:132-146) Analogon: `ComboBox` mit `model: root.audio-in-devices`, `current-index <=> root.audio-in-index`, `selected(index) => { root.audio-in-changed(index) }`, sichtbar nur wenn `root.has-audio-in` (neue `in property <bool> has-audio-in` — gui.rs setzt sie aus `!audio::Session`-Port-Info bzw. `loader::audio_port_channels(plugin, true)`).

- [ ] **Step 2: gui.rs verdrahten**

In `run`: `ui.set_audio_in_devices(ModelExt...)` via `audio::input_devices()`, `has_audio_in` setzen; Callback `on_audio_in_changed`: Index merken + `start_audio` neu aufrufen (droppt alte Session, siehe aura gui.rs:314); `start_audio` übergibt gewählten Input-Namen an `audio::open`.

- [ ] **Step 3: Smoke + commit**

```bash
cargo run -p clap-host-app -- --plugin /pfad/zu/plugin-mit-input.clap --gui
```
Erwartet: Input-Panel sichtbar, Wechsel des Input-Devices startet Audio neu. Effekt-Plugin mit Input testen. Dann:
```bash
git add -A && git commit -m "feat(app): audio input device picker in UI"
```

---

### Task 13: Host-Extensions ausbauen (host.rs)

**Files:**
- Modify: `crates/clap-host-core/src/host.rs`

**Interfaces:**
- Consumes: aktueller host.rs (Task 3, Stubs bei aura loader.rs:122-125, 137-143).
- Produces: `host::{take_params_dirty() -> bool`, `request_timer(plugin, period_ms) -> clap_id`, `cancel_timer(id)`, `take_timers_due(...)`, `set_latency(plugin, frames)`, `set_tail(plugin, frames)`-Tracking}. `host_get_extension` liefert zusätzlich `CLAP_EXT_STATE`, `CLAP_EXT_TIMER`, `CLAP_EXT_LATENCY`, `CLAP_EXT_TAIL`, `CLAP_EXT_NOTE_NAME`-Host-Structs.

- [ ] **Step 1: params-Stubs real machen**

`rescan`/`clear`/`request_flush` setzen bisher nur Flags (bzw. ignorieren). Jetzt: `params.rescan`/`clear` setzen `PARAMS_DIRTY`-Flag; `pump_main_thread` prüft Flag und ruft `param_value`-Refresh-Callback — Core-seitig als `take_params_dirty() -> bool` exponieren (die UI pollt ohnehin 20 Hz und liest neu). `request_flush` darf auf dem Audio-Thread liegen: Marker-Flag, das `Engine::process` am Blockanfang prüft und `params.flush(plugin, in=empty, out=sink)` aufruft.

- [ ] **Step 2: state-Extension (host-seitig)**

`clap_host_state`-Struct mit `mark_dirty`: setzt `STATE_DIRTY`-Flag; `take_state_dirty() -> bool` publizieren. In `host_get_extension` für `CLAP_EXT_STATE` ausliefern.

- [ ] **Step 3: timer-Extension**

Viele Windows-Plugins brauchen `clap_host_timer`. Registry: `static TIMERS: Mutex<Vec<(clap_id, u32 period_ms, Instant last)>>` + ein Hintergrund-Thread (gestartet in `make_host` via `Once`) tickt alle 5 ms, sammelt fällige Timer-Ids in einem `ArrayQueue`, `pump_main_thread` ruft `plugin->timer_support->on_timer(plugin, id)` (via `plugin_ext(plugin, CLAP_EXT_TIMER_SUPPORT)`) für fällige Ids. `register_timer` erzeugt Ids ab 1.

- [ ] **Step 4: latency / tail / note-name**

`clap_host_latency` (`changed` → Flag, `take_latency_changed()`), `clap_host_tail` (Flag analog). `clap_host_note_name` (changed → Flag). Alle in `host_get_extension` registrieren.

- [ ] **Step 5: Smoke mit echten Plugins + commit**

Mindestens ein AURA-Plugin + ein Third-Party-CLAP (z.B. ein OB-Xd/ Vital CLAP) laden; beobachten: keine fehlende-Extension-Warnungen mehr, Param-Automations sichtbar, Plugin-GUI lebt. Dann:
```bash
cargo build --workspace && git add -A && git commit -m "feat(core): real host extensions (params, state, timer, latency, tail, note-name)"
```

---

### Task 14: State save/load (`state.rs`) + CLI

**Files:**
- Create: `crates/clap-host-core/src/state.rs`
- Test: `crates/clap-host-core/src/state.rs` (`#[cfg(test)]`)
- Modify: `crates/clap-host-core/src/lib.rs`, `crates/clap-host-app/src/cli.rs`, `crates/clap-host-app/src/main.rs`

**Interfaces:**
- Consumes: `loader::plugin_ext`, ostream-Muster aus aura preset.rs:217-264 (Vorlage).
- Produces: `state::{save(plugin, path: &Path) -> Result<(), String>`, `load(plugin, path: &Path) -> Result<(), String>}`; CLI `--save-state <path>` (nach `--set`/Preset, vor `--play`) und `--load-state <path>` (nach `init`, vor `--set`).

- [ ] **Step 1: Failing tests — Stream-Puffer**

In `state.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_buffer_roundtrip() {
        let mut buf = StreamBuf::new();
        let out = buf.as_ostream();
        unsafe {
            ((*out).write)(out, b"hello".as_ptr().cast(), 5);
        }
        assert_eq!(buf.bytes(), b"hello");

        let inp = buf.as_istream();
        let mut readback = [0u8; 5];
        let n = unsafe { ((*inp).read)(inp, readback.as_mut_ptr().cast(), 5) };
        assert_eq!(n, 5);
        assert_eq!(&readback, b"hello");
    }
}
```

- [ ] **Step 2: Tests laufen — FAIL** (`StreamBuf` existiert nicht).

- [ ] **Step 3: Implementieren**

`StreamBuf { data: Vec<u8>, pos: usize }` mit `as_ostream()` (write-Callback appendet, `write_count` liefert geschriebene Anzahl) und `as_istream()` (read-Callback kopiert ab `pos`, liefert gelesene Anzahl, 0 am Ende) — C-Struct-Wrapper analog zu aura preset.rs:226-244. Dann:
```rust
pub fn save(plugin: *const clap_plugin, path: &Path) -> Result<(), String> {
    let state = plugin_ext(plugin, CLAP_EXT_STATE).ok_or("plugin has no state extension")?;
    let mut buf = StreamBuf::new();
    unsafe {
        ((*state.cast::<clap_plugin_state>().as_ref().unwrap()).save)(plugin.cast(), buf.as_ostream());
    }
    std::fs::write(path, buf.bytes()).map_err(|e| e.to_string())
}

pub fn load(plugin: *const clap_plugin, path: &Path) -> Result<(), String> {
    let state = plugin_ext(plugin, CLAP_EXT_STATE).ok_or("plugin has no state extension")?;
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut buf = StreamBuf::from(data);
    let ok = unsafe {
        ((*state.cast::<clap_plugin_state>().as_ref().unwrap()).load)(plugin.cast(), buf.as_istream())
    };
    if ok { Ok(()) } else { Err("plugin rejected state blob".into()) }
}
```
(exakte Feldnamen an clap-sys 0.5 `clap_plugin_state` anpassen; `plugin` als `*const clap_plugin` casten wie in preset.rs.)

- [ ] **Step 4: Tests grün + CLI-Flags**

```bash
cargo test -p clap-host-core state
```
`--save-state <path>` nach Preset-/Set-Verarbeitung, `--load-state <path>` direkt nach `init` (Reihenfolge in main.rs beachten). Smoke:
```bash
cargo run -p clap-host-app -- --plugin p.clap --set cutoff=0.8 --save-state /tmp/state.bin
cargo run -p clap-host-app -- --plugin p.clap --load-state /tmp/state.bin --list-params
```
Erwartet: gespeicherter Wert sichtbar.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core,app): plugin state save/load to files"
```

---

### Task 15: State + Dirty-Indicator in der UI

**Files:**
- Modify: `crates/clap-host-app/ui/host.slint`, `crates/clap-host-app/src/gui.rs`

**Interfaces:**
- Consumes: `state::{save, load}` (Task 14), `host::take_state_dirty` (Task 13).
- Produces: HostWindow-Callbacks `save-state()`, `load-state()` (öffnen Native-File-Dialog via `slint::spawn_local` + `rfd`? — Nein: YAGNI, kein neuer Dependency. Stattdessen: feste Pfade neben der Plugin-Datei, `<plugin>.state.bin`, Buttons "Save State"/"Load State" + dirty-Punkt im Header).

- [ ] **Step 1: host.slint**

Header: zwei Buttons `Save State` / `Load State`, `in property <bool> state-dirty` zeigt `●` neben dem Plugin-Namen.

- [ ] **Step 2: gui.rs**

Callbacks: `on_save_state` → `state::save(plugin, &default_state_path)` (Log-Zeile setzen); `on_load_state` → `state::load` + Param-Model refresh. Timer-Loop: `host::take_state_dirty()` → `ui.set_state_dirty(true)`; nach save/load zurücksetzen.

- [ ] **Step 3: Smoke + commit**

GUI: Param ändern → dirty-Punkt erscheint (Plugin muss `state.mark_dirty` rufen); Save → `*.state.bin` liegt neben Plugin; Load stellt Werte wieder her. Dann:
```bash
git add -A && git commit -m "feat(app): state save/load and dirty indicator in UI"
```

---

### Task 16: Hardening + Docs

**Files:**
- Modify: README.md
- Create: `docs/smoke-checklist.md`

- [ ] **Step 1: clippy streng + tests**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Alle pedantic-Warnungen beheben oder mit Begründung + `#[allow]` gezielt unterdrücken (bisher gesammelte Notizen aus Tasks 3-5 abarbeiten).

- [ ] **Step 2: README ausbauen**

Build/Test-Befehle, CLI-Flag-Übersicht, GUI-Kurzbeschreibung, Design-Hinweis (core/app-Grenze), Follow-ups: Embed (win32_embed aus aura), remote-controls, thread-pool, transport/timeline, Multi-Plugin-Graph.

- [ ] **Step 3: Smoke-Checkliste**

`docs/smoke-checklist.md`: nummerierte manuelle Checks (CLI laden/params/presets/play, scan, list-devices, GUI devices/params/keyboard/GUI-toggle/state) mit Abhaken-Spalte, je mit Beispiel-Plugin-Kategorie.

- [ ] **Step 4: Final commit**

```bash
git add -A && git commit -m "docs: readme, smoke checklist, clippy clean"
```

---

## Self-Review (Plan)

- **Spec coverage:** V1-Must-haves → Tasks: load/select plugin T3/T6; audio out+in T5/T8; MIDI+keyboard T4/T10 (Keyboard existiert in host.slint); CLI parity T6 + T7/T8/T14; Slint UI T9-T12, T15; floating GUI T11; host basics T3/T13/T14; scanner T7. Non-goals nicht eingeplant (embed, transport, multi-plugin) ✓.
- **Konsistenz Signatures:** `audio::open(plugin, device_name, input_name, midi_rx, ui_rx)` definiert in T8 und konsument in T10/T12. `gui::run(..., input_name)` T10, genutzt in T6-Step 2 (`--gui`-Pfad) — dort Input-Name aus `--input-device` durchreichen. `HostWindow`-Properties erweitert in T12/T15 ohne Bruch der T9-API (additive). `plugin_gui` in Core ab T11; gui.rs vorher referenziert `FloatingGui` — T10 muss den Import bereits als `clap_host_core::plugin_gui` schreiben (steht so im Step) ✓.
- **Platzhalter:** keine TBDs; alle neuen Dateien haben vollständigen Code oder exakte Quellverweise (Zeilennummern aura-host) + edit-Instruktionen.
