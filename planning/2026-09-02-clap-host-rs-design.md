# CLAP-Host-RS — Design Spec & Implementation Plan

## Summary

Standalone Rust CLAP host in `L:\CLAP-Host-RS`: Plugin-Dev-Host, Codebasis **kopiert** aus [aura-host](https://github.com/LX-Audiolabs/aura/tree/main/crates/aura-host) (Aura bleibt unberührt), wächst Richtung Feature-Parität mit [free-audio/clap-host](https://github.com/free-audio/clap-host) und darüber hinaus (u.a. wählbares Audio-IN, Plugin-Scanner).

## Decisions

| Thema | Entscheidung |
|-------|----------------|
| Startpunkt | aura-host **kopieren**, nicht aus Aura extrahieren |
| Primärziel | Plugin-Dev-Host |
| Sekundär | Möglichst viel clap-host + Erweiterungen |
| GUI | Slint |
| Plattform V1 | Win/macOS/Linux Audio+MIDI; Plugin-GUI **nur floating** |
| Layout | Workspace: `clap-host-core` + `clap-host-app` |
| Ansatz | A: kopieren → lauffähig → wachsen |
| Scanner | **In V1** (CLAP-Standardpfade) |
| Lizenz | **MIT** — AURA ist GPL-3.0, gilt nur dort, nicht übernehmen |
| Rust | 1.92, edition 2024 (AURA-Gleichstand), resolver "3" |
| Build | `slint-build =1.17.1` statt `lx-aura-build`; `@aura`-Widget-Import in `host.slint` wird durch eigenes Theme + `ParamSlider` ersetzt (Quelle: `crates/aura-build/ui/theme.slint` + `slider.slint` als Vorlage) |
| Profiles | `panic = "unwind"` übernehmen (Host-Boundary `catch_unwind`), clippy `pedantic = warn` |

## Architecture

```
CLAP-Host-RS/
  Cargo.toml                 # workspace
  README.md
  LICENSE
  crates/
    clap-host-core/          # library
      src/
        lib.rs
        loader.rs            # from aura-host
        host.rs              # clap_host + extensions (grow)
        audio.rs             # cpal out + in session
        events.rs
        midi.rs
        preset.rs
        state.rs             # NEW: state save/load files
        scan.rs              # NEW: standard path scanner
    clap-host-app/           # binary
      Cargo.toml
      build.rs               # slint::compile (no lx-aura-build)
      ui/                    # from aura-host, adapted
      src/
        main.rs              # CLI + GUI entry
        gui.rs
        plugin_gui.rs        # floating only
```

### Crate boundaries

- **core**: no Slint. Load/create/activate plugin, audio session, MIDI→events, params, presets, state, scan. Unit-testable without GUI.
- **app**: CLI flags, Slint shell, device pickers (out/in/MIDI), param controls, keyboard notes, opens floating plugin GUI via core helpers.

### Port mapping from aura-host (Analyse 2026-09-02)

- `events.rs` (235 LOC, inkl. Tests) → core, direkt kopierbar.
- `loader.rs` (494 LOC) → core, aufgeteilt: clap_host-Callbacks (Zeilen 39–203, inkl. `make_host`, Threading-Pumpen) → neues `host.rs`; Rest → `loader.rs`. Identity-Strings rebranden (`loader.rs:186-188`, `preset.rs:100-101`, `midi.rs:16,37`).
- `audio.rs` (416 LOC) → core; `run()` (CLI-Helfer) → app. `open_input` (audio.rs:356) hart auf `default_input_device()` — wird in Phase 2 per Gerätenamen parametrisiert.
- `midi.rs` (78 LOC), `preset.rs` (264 LOC) → core, direkt kopierbar + rebranden. `preset::pull` (ostream-Muster) ist die Vorlage für `state.rs`.
- `plugin_gui.rs` (80 LOC, Slint-frei, floating win/mac/linux) → **core** (Helpers für die App).
- `gui.rs` (347 LOC) + `ui/host.slint` (219 LOC) → app. Computer-Keyboard-Notes existieren bereits (FocusScope in host.slint:53-78) — portieren, nicht neu bauen. Audio-Input-Picker fehlt → Phase 3.
- `main.rs` (231 LOC) → app, umschreiben gegen Core-Lib.
- Host-Extensions-Stand: log + thread-check echt, gui/params = Stubs. Fehlen für Phase 4: state (host-seitig), timer, latency, note-name; Output-Events werden aktuell komplett gesunken (`sink_output_events`).

### Explicitly omitted from V1 copy

- `win32_embed.rs` / SetParent embed — inkl. Abhängigkeiten `windows-sys` und `raw-window-handle` (fällt ganz weg, floating braucht kein eigenes HWND)
- `lx-aura-build` / workspace lints from Aura
- Aura path dependencies
- CLI-Helfer mit Seiteneffekten wandern aus dem Core in die App: `list_params`/`list_ports`/`print_list` (`println!`, `process::exit`), `audio::run`

## Data flow / lifecycle

1. Path or scanner pick → `Loader::open` → factory → create → init
2. Optional param sets / preset / state load
3. `Session::open`: default or named **output**; if plugin has input ports → named/default **input** capture ring
4. MIDI port → lock-free queue → audio callback
5. Floating CLAP GUI create/show; host Slint UI parallel
6. Audio thread: capture → process (no alloc) → interleave out
7. Main thread: host callbacks, UI↔engine queues
8. Shutdown: destroy GUI → drop streams → stop/deactivate/destroy/unload

## V1 scope

**Must have**

- Load `.clap`, select plugin by id/index
- Audio out + audio in (device selectable in UI when relevant; engine already supports capture)
- MIDI in + computer-keyboard notes
- CLI parity with aura-host (`--gui`, `--play`, `--list-params`, `--list-presets`, `--set`, `--midi-in`, …)
- Slint host UI: devices (out, in, MIDI), params
- Floating plugin GUI (win/mac/linux APIs)
- Host basics: log, thread-check, params get/set, state save/load to file
- **Plugin scanner**: CLAP standard search paths per OS, list in CLI + GUI

**Non-goals V1**

- Parent-window embed
- Remote-controls UI, posix-fd, thread-pool, misbehaviour-terminate (clap-host maximal checking)
- Transport/timeline, multi-plugin graph
- Keeping code in sync automatically with aura-host (manual cherry-pick if desired later)

**Early improvements vs references**

- Audio input device exposed in UI (aura: engine yes / UI weak; clap-host: settings model)
- No Qt/vcpkg — pure Rust toolchain
- `clap-host-core` usable without GUI

## Dependencies (initial)

- `clap-sys`, `libloading`
- `cpal`, `midir`, `crossbeam-queue`
- app: `slint` (winit + femtovg, similar feature set to aura-host desktop)
- Windows: only what floating GUI needs (no SetParent stack required for V1)

## Testing

- Core unit tests: event list, scan path helpers, param helpers
- Optional integration: load a fixture `.clap` if available in CI
- Manual: third-party CLAPs + LX Aura plugins

## Implementation phases

### Phase 0 — Scaffold

- `cargo init` workspace, two crates, README, `.gitignore`, LICENSE (default MIT)
- Empty `lib.rs` / `main.rs` build

### Phase 1 — Port core from aura-host

- Copy `loader`, `events`, `midi`, `audio`, `preset` into `clap-host-core`
- Strip Aura-only bits; public API surface cleaned
- CLI binary without Slint: list plugins, list params, `--play`

### Phase 2 — Scanner + Audio-IN UX in core/CLI

- `scan.rs`: OS standard CLAP directories (Windows: `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`; macOS: `/Library/Audio/Plug-Ins/CLAP`, `~/Library/Audio/Plug-Ins/CLAP`; Linux: `/usr/lib/clap`, `/usr/local/lib/clap`, `~/.clap`) — Pfadliste gegen CLAP-Doku verifizieren, rekursiv `*.clap` sammeln
- CLI: `--scan`, `--list-devices` (out/in/MIDI)
- Audio-IN: `input_devices() -> Vec<String>` (Analogon zu `output_devices()`, audio.rs:224); `Session::open` bekommt zusätzlich `input_name: Option<&str>`; `open_input` matched per `device_label` statt `default_input_device()` (audio.rs:356-363)

### Phase 3 — Slint app

- Port `ui/` + `gui.rs` + `plugin_gui.rs` (floating only)
- `build.rs` with `slint_build` / `slint::compile`
- Wire device pickers including input

### Phase 4 — Host extensions + state

- Expand `clap_host` callbacks toward clap-host (log, params rescan/flush, state dirty, timers as needed for real plugins)
- State save/load files in CLI + UI

### Phase 5 — Hardening

- Docs, smoke checklist, fix plugin quirks discovered manually
- Note follow-ups: embed, remote controls, thread pool

## Spec self-review

- [x] No TBD placeholders left for V1 decisions
- [x] Architecture matches “copy aura → core+app → grow”
- [x] Scanner included in V1 per user
- [x] Embed explicitly out of V1
- [x] Audio-IN: engine path exists; UI/device select is explicit work
- [x] Scope fits one implementation plan (phased, not multiple products)

## Open (minor, can default)

- ~~License~~: **MIT** (entschieden, siehe Decisions)
- Package publish: `publish = false` initially
- Crate versions: `0.1.0`
- Host-Identity-Strings: `"CLAP-Host-RS"` / `"lxndrbe"` / `https://github.com/lxndrbe/CLAP-Host-RS`
