# Smoke checklist — CLAP-Host-RS

Manual checks after touching the audio path, the loader or the UI. Example
plugins: any synth with a factory GUI (e.g. Vital), the AURA `smoke_synth`,
and an effect/utility plugin for the audio-input check. Run each check and
tick the box.

Notation: `HOST` = `target/debug/clap-host-rs` (or `target/release/clap-host-rs`),
`SYNTH` = path to a synth `.clap`/`.dll`, `FX` = path to an effect plugin.

## CLI

1. [ ] **Load + list**: `HOST --plugin SYNTH` prints `found N plugin(s)` and
       the descriptors (name + id). With several plugins in one file,
       `--id <clap-id>` picks the right one.
2. [ ] **Params**: `HOST --plugin SYNTH --list-params` lists every visible
       parameter with current value, plugin-formatted text and range.
3. [ ] **Set**: `HOST --plugin SYNTH --set <id>=<val> --list-params` shows the
       new value (applied while deactivated, so it sticks).
4. [ ] **Presets**: `HOST --plugin SYNTH --list-presets` lists factory presets.
5. [ ] **Pull preset**: `HOST --plugin SYNTH --pull-preset <key> --out preset.clap`
       writes a file; loading that file in another host works.
6. [ ] **Play (synth)**: `HOST --plugin SYNTH --play` starts the stream
       (`playing at 48000 Hz, 2ch`), MIDI notes sound, ctrl+c exits cleanly —
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
11. [ ] **MIDI**: `HOST --plugin SYNTH --play --midi-in <port>` prints
        `listening`-style status and notes from that port sound.

## GUI

12. [ ] **Start**: `HOST --plugin SYNTH --gui` opens the window titled
        "CLAP-Host-RS"; the status line shows rate/channels/ports.
13. [ ] **Devices**: switching the output device in the picker restarts audio
        (status line updates, sound continues); with an audio-input plugin the
        input picker changes the capture source.
14. [ ] **Params**: moving a slider changes the parameter in real time
        (audible in the synth); the value text follows.
15. [ ] **Keyboard**: with the window focused, computer-keyboard keys trigger
        notes; releasing stops them.
16. [ ] **Plugin GUI**: the toggle opens the plugin's own (floating) window;
        closing it from the plugin side disables the toggle again.
17. [ ] **State save/load**: the buttons write/read `<plugin>.state.bin`;
        after a parameter change the dirty dot appears, save clears it, load
        restores the values in the sliders.

## Regression guards

18. [ ] `cargo test --workspace` — all green.
19. [ ] `cargo clippy --workspace --all-targets -- -D warnings` — silent.
