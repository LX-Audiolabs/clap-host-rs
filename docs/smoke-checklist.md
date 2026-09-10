# Smoke checklist — CLAP-Host-RS

Manual checks to run after touching the audio path, the loader, or the UI.
Example plugins: any synth with a factory GUI (e.g. Vital), the AURA `smoke_synth`,
and an effect/utility plugin for the audio-input check. Run each check and
tick the box.

Notation: `HOST` = `target/debug/clap-host-rs` (or `target/release/clap-host-rs`),
`SYNTH` = path to a synth `.clap`/`.dll`, `FX` = path to an effect plugin.

## CLI

1. [ ] **Load + list**: `HOST --plugin SYNTH` prints `found N plugin(s)` and the
       descriptors (name + id). When a file contains multiple plugins,
       `--id <clap-id>` selects the correct one.
2. [ ] **Params**: `HOST --plugin SYNTH --list-params` shows every visible
       parameter with value, formatted text and range.
3. [ ] **Set**: `HOST --plugin SYNTH --set <id>=<val> --list-params` shows the
       updated value (applied while deactivated, so it persists).
4. [ ] **Presets**: `HOST --plugin SYNTH --list-presets` lists factory presets.
5. [ ] **Pull preset**: `HOST --plugin SYNTH --pull-preset <key> --out preset.clap`
       writes a file; loading that file in another host works.
6. [ ] **Play (synth)**: `HOST --plugin SYNTH --play` starts the stream
       (`playing at 48000 Hz, 2ch`), MIDI notes sound, Ctrl+C exits cleanly —
       no `wrong thread` / `host-misbehaving` log lines from the plugin.
7. [ ] **Play (effect)**: `HOST --plugin FX --play --input-device <name>`
       routes the chosen capture device into the plugin's input ports
       (`audio in: <device>` line); the processed signal is audible.
8. [ ] **State round-trip**: `HOST --plugin SYNTH --set <id>=<val> --save-state s.bin`,
       then `HOST --plugin SYNTH --load-state s.bin --list-params` shows the
       restored value.
9. [ ] **Scan**: `HOST --scan` walks the standard CLAP dirs, lists plugins,
       warns (but continues) on broken binaries.
10. [ ] **List devices**: `HOST --list-devices` prints output devices, input
        devices and MIDI ports.
11. [ ] **MIDI**: `HOST --plugin SYNTH --play --midi-in <port>` prints a
        `listening`-style status and notes from that port are audible.

## GUI

12. [ ] **Start**: `HOST --plugin SYNTH --gui` opens the window titled
        "CLAP-Host-RS"; the status line shows sample rate/channels/ports.
13. [ ] **Devices**: changing the output device in the picker restarts audio
        (status updates, sound continues); for audio-input plugins the input
        picker changes the capture source.
14. [ ] **Params**: moving a slider updates the parameter in real time
        (audible in the synth); the displayed value follows.
15. [ ] **Keyboard**: with the window focused, computer keyboard keys trigger
        notes; releasing stops them.
16. [ ] **Plugin GUI**: the toggle opens the plugin's floating window; if the
        plugin only supports an embedded GUI, the editor appears inside the
        host window; closing it from the plugin side turns the toggle off.
17. [ ] **Remote controls**: a plugin with `clap.remote-controls` shows the
        page panel (page name + ‹/›); ‹/› page through the pages and the
        sliders drive the parameters.
18. [ ] **Setup**: the Setup button opens the dialog; changing a device
        restarts audio; the previous selection is still selected when the
        dialog is reopened.
19. [ ] **State save/load**: the buttons write/read `<plugin>.state.bin`;
        after a parameter change the dirty dot appears, save clears it, load
        restores the slider values.
20. [ ] **Preset laden**: `HOST --plugin SYNTH --load-preset <file>` plus the
        GUI "Load Preset…" button — the sliders show the preset values; the
        error case shows the plugin's message in the log.
21. [ ] **Embedded-Resize**: Surge-XT zoom changes the editor size — the slot
        and the host window grow with it, nothing is clipped.
22. [ ] **Show/Hide**: when the plugin sends `request_hide`/`request_show`,
        the editor disappears/reappears without a crash (floating + embedded).

## Regression guards

23. [ ] `cargo test --workspace` — all tests pass.
24. [ ] `cargo clippy --workspace --all-targets -- -D warnings` — no warnings.
25. [ ] **Param-Feedback**: Regler im Plugin-Editor bewegen → Host-Slider folgt
        innerhalb ~50 ms.
