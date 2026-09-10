# CLAP-Host-RS — Optimization Ideas (Backlog, ungeplant)

Sammlung von Verbesserungsideen jenseits von [2026-09-02-clap-host-rs-design.md](2026-09-02-clap-host-rs-design.md).
Kein Commitment, keine Reihenfolge — Ideenpool zum Abgreifen, wenn Kapazität da ist.

## Audio-Geräte

### Kanal-Zuordnung für In/Out (statt fix Kanal 0..N)

**Problem:** `audio.rs::interleave` (audio.rs:263-271) mapped Plugin-Output immer
auf Device-Kanal `0..device_channels` — bei einem Interface mit z. B. 18 Ausgängen
landet der Host immer auf Kanal 1-2. Kein Weg, stattdessen z. B. Kanal 5-6 zu nutzen
(gängiger Fall: Kopfhörerausgang liegt nicht auf 1-2, oder man will parallel zu einer
DAW auf ungenutzte Kanäle routen). Gleiches Problem spiegelverkehrt beim Input
(`open_input`, audio.rs:459+ — nimmt `device_label` für die Geräteauswahl, aber keine
Kanalauswahl innerhalb des Geräts).

**Idee:** Pro Device zusätzliches Dropdown "Kanäle", befüllt aus
`SupportedStreamConfig`/`device.supported_output_configs()` — Optionen wie
`1-2`, `3-4`, `5-6`, … je nach `device_channels`. Auswahl liefert einen Kanal-Offset,
den `interleave()` (Output) bzw. das Capture-Remixing (Input) zusätzlich zum
bestehenden `device_channels`-Wert berücksichtigt.

- Alternative/Ergänzung zur Checkbox-Idee: statt einzelner Checkboxen pro Kanal
  ("welche sind aktiv") ein Dropdown mit Kanal-*Paaren* passend zur Portzahl des
  Plugins (2 für Stereo-Output, 1 für Mono-Input etc.) — einfacher zu bedienen als
  N Checkboxen, deckt den Hauptfall (Interface mit vielen Kanälen, Host soll auf ein
  bestimmtes Paar) ohne Kombinatorik-Explosion ab.
- Aufwand: mittel. `Session::open`/`open_input` brauchen einen `channel_offset:
  usize`-Parameter zusätzlich zu `input_name`/Device-Name; UI braucht ein zweites
  Combobox-Property + Callback analog zu `audio-in-changed` (gui.rs:182).
- Betrifft: `crates/clap-host-core/src/audio.rs`, `crates/clap-host-app/src/gui.rs`,
  `crates/clap-host-app/ui/host.slint`, CLI (`--output-device`/`--input-device` um
  `:channel` Suffix erweitern, z. B. `--output-device "RME Fireface:5-6"`).

### Sample-Rate / Buffer-Size wählbar — ✅ ERLEDIGT

Aktuell nutzt `open()` immer `device.default_output_config()` (audio.rs:365).
Manche Plugins/Workflows wollen 44.1 kHz statt Gerätedefault, oder eine kleinere
Blockgröße für Latenz-Tests. `supported_output_configs()` liefert die Range;
UI-Dropdown analog zur Kanalauswahl.

**Erledigt (Stand 2026-09-03):** `StreamSettings { sample_rate, buffer_size }`
durchgereicht von CLI (`--sample-rate`, `--buffer-size`) und Setup-Dialog (zwei
neue ComboBoxes, pro Gerät neu befüllt); fixe Buffer-Size mit Fallback auf
Backend-Default wenn das Backend sie ablehnt; ungültige Rate = sauberer Fehler.

### ASIO-Backend (Windows) — ✅ ERLEDIGT (Feature, SDK beim Nutzer)

cpal unterstützt ASIO über Feature-Flag + ASIO-SDK-Pfad. WASAPI (aktueller Default)
reicht für Dev-Zwecke, aber Latenzvergleich mit echten Interfaces braucht oft ASIO.
Aufwand: baubar, aber Lizenz-/SDK-Beschaffung (Steinberg ASIO SDK) ist der lästige Teil,
kein reines Code-Thema.

**Erledigt (Stand 2026-09-03):** Optionaler Cargo-Feature `asio`
(`--features asio`, mapped auf `cpal/asio`), default aus. Build-Voraussetzungen
dokumentiert: `CPAL_ASIO_DIR` → Steinberg ASIO SDK, LLVM/Clang für bindgen.
SDK-Akquise (Steinberg-Lizenz) bleibt Nutzer-seitig — nicht verifiziert, kein
ASIO-Gerät hier.

## GUI / UX

### UI-Redesign: Setup-Dialog statt Inline-Picker (Grundsatzentscheidung) — ✅ ERLEDIGT

**Problem:** `host.slint` wächst inline — Output/Input/MIDI-Device-Picker,
Kanalauswahl (falls oben kommt), Sample-Rate/Buffer-Size (falls oben kommt),
Save/Load-State-Buttons, Remote-Controls-Paging (unten), Preset-Browser (unten),
Param-Suche (unten) landen alle im selben Hauptfenster. Wird schnell überladen,
besonders bei Plugins mit vielen Params (Vital: 775 Zeilen Liste) — genau der
Fall, wo ein aufgeräumtes Hauptfenster am wichtigsten wäre.

**Idee:** Alles, was mit Audio-/MIDI-*Setup* zu tun hat (Output/Input-Device,
Kanalauswahl, Sample-Rate/Buffer-Size, MIDI-Port), hinter einen einzigen
"Setup"-Button im Header packen, der ein Popup/Dialog-Fenster öffnet
(`slint::Dialog`-artiges zweites `Window`-Component oder ein Overlay im selben
Fenster — Slint hat kein natives modales Dialog-Widget, zweites `Window`
exportieren und `.show()` reicht). Hauptfenster bleibt dann: Plugin-Name/Titel,
Param-Liste (+ Suche), Save/Load State, Remote-Controls-Bereich, Keyboard.

- Referenz-Host macht's ähnlich: eigener `SettingsDialog` (`settings-dialog.hh`)
  getrennt vom `MainWindow`, dort leben Audio-/MIDI-Settings-Widgets
  (`audio-settings-widget.hh`, `midi-settings-widget.hh`).
- Aufwand: mittel-groß, echtes UI-Redesign statt Feature-Zusatz — eigener
  Slint-Component `SetupDialog` in neuer `.slint`-Datei, `gui.rs` verkabelt
  dessen Callbacks statt die des `HostWindow` direkt für Device-Wechsel.
  Sollte VOR den Kanal-/Sample-Rate-Ideen oben umgesetzt werden, sonst wächst
  das Hauptfenster erst recht und der Dialog muss eh nachträglich alles absaugen.
- Reihenfolge-Empfehlung: Setup-Dialog zuerst (reines Refactoring der
  bestehenden Picker, kein neues Feature) → danach Kanalauswahl/Sample-Rate
  direkt im neuen Dialog ergänzen, nicht mehr im Hauptfenster.

### Param-Liste: Suche/Filter — ✅ ERLEDIGT

Bei Plugins mit vielen Parametern (Vital: 775) ist die Slint-Liste unhandlich lang.
Ein simples Textfeld, das `param_rows` (gui.rs) nach Label filtert, wäre günstig
(kein neues Widget nötig, `std-widgets` `LineEdit` reicht).

**Erledigt (Stand 2026-09-03):** `LineEdit` über der Liste (two-way binding auf
`param-filter`), Filter case-insensitive auf Param-Name; Timer rebuildet das
Modell per `set_vec` wenn der Filter sich ändert (shrinking Filter verwirft
Zeilen, der Diff-Loop im Poll macht das nicht).

### Preset-Browser in der GUI

Aktuell nur CLI (`--list-presets`, `--pull-preset`). Ein einfaches Listenpanel in
host.slint, das `preset::list`/`pull` aufruft, würde den Workflow "Preset durchhören"
GUI-tauglich machen ohne CLI-Umweg.

### Remote-Controls Paging-UI — ✅ ERLEDIGT

War in der Design-Doc als V1-Non-Goal gelistet ("Remote-controls-UI"), wird
jetzt aufgenommen — Referenz-Host implementiert das vollständig
(`host/plugin-quick-controls-widget.cc/hh` + `plugin-host.cc`:
`remoteControlsChanged`, `remoteControlsSuggestPage`, Page-Index-Verwaltung
über `_remoteControlsPages`/`_remoteControlsPagesIndex`). Extension:
`clap.remote-controls` — Plugin gruppiert seine wichtigsten Params in
nummerierte "Pages" (je bis zu 8 Params), Host zeigt eine Page auf einmal mit
Vor/Zurück-Navigation statt der kompletten (potenziell hunderte Einträge
langen) Param-Liste.

- **Host-Extension neu**: `clap_host_remote_controls` in `host.rs` (analog zu
  bestehendem State/Timer/Latency-Muster) — `changed()`-Callback setzt ein
  `REMOTE_CONTROLS_DIRTY`-Atomic (wie `STATE_DIRTY`), Host fragt dann
  `clap_plugin_remote_controls.{count, get}` ab.
- **UI**: eigener Bereich/Panel im Hauptfenster (oder eigenes Tab neben der
  vollen Param-Liste, siehe Setup-Dialog-Idee oben — Param-Suche +
  Remote-Controls-Paging sind beides Antworten auf "zu viele Params", könnten
  sich als zwei Tabs im selben Panel ergänzen statt konkurrieren).
- Aufwand: mittel. Neue Extension host-seitig (`clap-sys::ext::remote_controls`
  prüfen ob vorhanden), neues Slint-Panel mit Page-Navigation
  (Vor/Zurück-Buttons + 8 Param-Slots, wiederverwendet `ParamSlider`).
- Nicht alle Plugins implementieren es (Vital/Surge XT ungeprüft) — Panel nur
  zeigen, wenn `clap.remote-controls` vorhanden (analog `gui-available`-Muster).

### Host-Extension-Lücken ggü. Referenz-Host (`free-audio/clap-host`)

Fund aus Quellcode-Vergleich (`host/plugin-host.cc`, 2026-09-02):

- **`gui.request_resize`** — ✅ ERLEDIGT (2026-09-10): Plugin bittet Host, sein
  (Floating-)Fenster live umzugrößen. Implementiert als Plugin→Host-Request für
  den Embedded-Editor (Slot + Host-Fenster wachsen mit, GUI-Poll zieht den
  Atomic); Floating: No-op. Weiterhin offen: Host-Window-Drag → Socket
  wächst nicht in den Editor.
- **`gui.request_show` / `gui.request_hide`** — ✅ ERLEDIGT (2026-09-10):
  Plugin darf Editor anfordern/verstecken; Upstream-Parität, Floating +
  Embedded.
- **`clap_plugin_preset_load`** — ✅ ERLEDIGT (2026-09-10): Host-Extension
  (`clap_host_preset_load` mit `loaded`/`on_error`), `load_file` im Core,
  CLI `--load-preset <file>` und GUI-Button "Load Preset…" (nativer
  Dateidialog, nur aktiv wenn das Plugin `clap.preset-load` implementiert).
- **Output-Events (`clap_process.out_events` / `params.flush`)** — ✅ ERLEDIGT
  (2026-09-10): Plugin→Host-Param-Feedback wurde bisher verworfen — Regler im
  Plugin-Editor haben Host-Slider nicht aktualisiert. Jetzt: Core fängt
  `PARAM_VALUE`/`GESTURE_BEGIN`/`GESTURE_END` aus den Output-Listen von
  `process()` und `params.flush` in einer lock-free `PluginOutEvent`-Queue;
  der GUI-Poll drained sie per Timer und spiegelt Werte in Param-Sliders und
  Remote-Controls.
- **Verweis:** `clap.thread-pool` und `clap.posix-fd-support` haben eigene
  Pläne unter `.superpowers/` — hier nicht weiter verfolgt.
- **"Provide Cookie"-Toggle** — ✅ ERLEDIGT: `ParamInfo.cookie` wird aus
  `clap_param_info` übernommen und in Param-Events (Audio-Thread + `set_param`)
  zurückgespiegelt; Plugins mit Cookie-Lookup brauchen keinen langsamen
  id-Lookup mehr.
- **Settings-Persistenz** bestätigt als Standard-Host-Feature (nicht nur
  unsere Idee) — siehe "Zuletzt genutzte Geräte/Kanäle merken" unten,
  Referenz macht's über `QSettings` + `AudioSettings`/`MidiSettings`.

### Zuletzt genutzte Geräte/Kanäle merken

Kleine Konfigdatei (`~/.config/clap-host-rs/settings.toml` o. ä.) mit letztem
Output/Input/MIDI-Device (+ Kanalauswahl, falls obige Idee kommt), damit man beim
nächsten Start nicht jedes Mal neu auswählen muss. YAGNI-Warnung: erst sinnvoll,
wenn Kanalauswahl oder viele Geräte das Neu-Einstellen wirklich nervig machen.

## Kompatibilität

### Vital.clap Absturz bei Param-Änderung (offen, ungeklärt)

Reproduzierter Crash (0xc0000005, Fault-Modul `Vital.clap`, nicht `clap-host-rs.exe`)
beim Ändern eines Parameters über die GUI (Slider-Drag und Fokus+Pfeiltaste, beide
Male reproduziert). Host-seitiger Event-Pfad (`audio.rs:179-181`, `events.rs::push_param`)
sieht spec-konform aus (Zeit=0, note_id/port/channel/key=-1, Anwendung im
Audio-Thread via `process()`). Root Cause nicht verifiziert — bräuchte WinDbg/Minidump-
Analyse, um zu klären, ob es ein Vital-Bug ist oder der Host in einem Burst-Fall
(viele Events in einem Block, kein Cap beim Drain von `ui_rx`) etwas tut, das ein
schlecht gehärtetes Plugin zum Absturz bringt. Surge XT übersteht denselben Test
(Slider-Drag) sauber — spricht eher für Vital-seitiges Problem, aber nicht bewiesen.

Mögliche Härtung unabhängig von der Ursache: `ui_rx`-Drain pro Block cappen
(z. B. max. 32 Events/Block, Rest bleibt in der Queue für den nächsten Block) —
spec-legal in beide Richtungen, reduziert aber die Burst-Größe, falls das der Trigger ist.

**Update 2026-09-03:** Härtung umgesetzt — `ui_rx`-Drain auf 32 Events/Block
gecapped (`audio.rs`, Rest bleibt queued). Root Cause weiterhin offen; Crash
mit Vital ggf. retesten.

### Embed-GUI (Parent-Window) für Embed-only-Plugins — ✅ ERLEDIGT

Vital und Surge XT antworten beide `is_api_supported(floating=true) == false` —
sie können nur eingebettet werden, nicht als eigenes Top-Level-Fenster (geprüft per
Debug-Instrumentierung in `plugin_gui.rs::supports_floating`, 2026-09-02). Das war
laut Design-Spec explizit V1-Non-Goal, ist jetzt umgesetzt (inkl. Kurskorrektur
darunter).

**Erledigt (Stand 2026-09-03):**

- `win32_embed.rs` im Core: Socket-HWND (`WS_CHILD`), `create` → `set_parent` →
  `show`, Drop macht `hide` + `destroy` + `DestroyWindow`.
- Reaper-Style-Layout: "Open plugin GUI" tauscht die Param-Seite gegen den
  Embedded-Editor (Top-Bar bleibt), Fenstergröße wird angepasst und bei Close
  wiederhergestellt.
- Größen-Kappung auf Monitor-Work-Area; Plugin wird per `gui.set_size` verkleinert
  wenn's nicht passt (Verhandlung *nach* `gui.create` — Vital antwortet `get_size`
  vorher nur mit Fallback).
- **Offen geblieben:** Host-Window-Drag wächst nicht in den Socket
  (Reverse-Richtung von `gui.request_resize`, siehe Extension-Lücken oben —
  Plugin→Host ist erledigt, Host→Plugin-Resize nicht).

**TODO (morgen): Umsetzungsskizze Win32-Embed** — ✅ UMGESETZT (siehe oben;
Skizze unten nur noch als Dokumentation des ursprünglichen Plans).

AURA hat das bereits fertig gelöst (`crates/aura-host/src/win32_embed.rs`, 175 LOC,
bewusst aus V1-Kopie ausgeschlossen — Annahme "floating reicht" war falsch, siehe
oben). Plan: rebranden + adaptieren statt neu entwickeln.

1. **Eigenes HWND holen**: `raw-window-handle`-Crate + Slints
   `ui.window().window_handle()` → `HasWindowHandle::window_handle()` →
   `RawWindowHandle::Win32(h)` → `HWND`. Vorlage: AURA `gui.rs::parent_hwnd`
   (aura-host/src/gui.rs:43-53).
2. **Socket-Fenster**: `windows-sys::CreateWindowExW` mit `WS_CHILD | WS_VISIBLE`,
   Parent = unser HWND, Größe aus `gui.get_size` (Fallback 400×300). Eigene
   Fensterklasse reicht mit `DefWindowProcW` (Plugin zeichnet selbst rein).
3. **Plugin einhängen**: `gui.create(plugin, WIN32, floating=false)` →
   `gui.set_parent(&clap_window{ win32: socket })` → `gui.show()`.
4. **Host-Fenster resizen** (optional, AURA macht's): `ui.window().set_size(...)`
   nach erfolgreichem Open, damit der Editor nicht abgeschnitten wird.
5. **Cleanup** (`Drop`): `gui.hide` + `gui.destroy`, dann `DestroyWindow(socket)`.

**Deps neu**: `windows-sys`, `raw-window-handle` — genau die zwei, die die
Design-Doc unter "Explicitly omitted from V1 copy" gestrichen hatte.

**Wiring in `gui.rs`**: `on_toggle_gui` (gui.rs:206-224) erweitern —
Floating zuerst versuchen (`plugin_gui::supports_floating`), sonst Embedded
(`win32_embed::supports_embedded`), analog AURA:
`has_gui = supports_floating(plugin) || supports_embedded(plugin)` für
`gui-available`. `PluginWindow`-Enum (`Floating` / `Embedded`) statt nur
`FloatingGui` in `Host`-Struct.

**Scope**: Windows-only zuerst (Dev-Plattform, AURA-Präzedenzfall 1:1 übertragbar).
macOS (NSView/Cocoa) und Linux (X11 XEmbed) sind eigene, größere Baustellen —
separate Follow-ups, nicht Teil dieses Tickets.

**Aufwand**: mittel. Reine Portierung + Verdrahtung, keine neue Architektur.

## Bereits in README "Follow-ups" gelistet (hier nicht dupliziert)

`CLAP_PATH`-Env im Scanner, Symlink-Zyklen-Guard, `"(none)"`-Eintrag im
Input-Picker, Thread-Pool, Transport/Timeline, Multi-Plugin-Graph,
Parent-Window-Embed (siehe oben, hier mit konkretem Befund ergänzt statt nur
genannt).

**Update 2026-09-03:** `CLAP_PATH` ✅ (`scan.rs`, `;`/`:`-getrennt, Spec),
`"(none)"`-Eintrag im Audio-Input-Picker ✅ (Setup-Dialog, Reihe 0),
Symlink-Zyklen-Guard ✅ (Canonicalisierung + visited-Set pro Scan,
Unix-Cycle-Test). Offen: Thread-Pool, Transport/Timeline, Multi-Plugin-Graph.

**Kurskorrektur:** Remote-Controls-Extension war hier als Non-Goal gelistet,
ist jetzt oben ("Remote-Controls Paging-UI") aktiv aufgenommen — nicht mehr
deferred.
