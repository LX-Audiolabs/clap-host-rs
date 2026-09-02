//! Hand-rolled CLI parser — no CLI library (parity with aura-host, YAGNI).

use std::path::PathBuf;

pub const USAGE: &str = "usage: clap-host-rs --plugin <path.clap> [--id <clap-id>] \
                         [--list-params] [--list-presets] \
                         [--pull-preset <key> --out <file>] [--set <id>=<val>]... \
                         [--play [--output-device <name>]] [--list-midi] [--midi-in <name>]";

// A CLI flag struct is exactly the case this lint doesn't help — each bool
// is an independent switch, not related state that wants an enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct Args {
    pub plugin_path: Option<String>,
    pub plugin_id: Option<String>,
    pub play: bool,
    pub output_device: Option<String>,
    pub list_params: bool,
    pub list_presets: bool,
    pub pull_preset: Option<String>,
    pub pull_out: Option<PathBuf>,
    pub sets: Vec<(u32, f64)>,
    pub list_midi: bool,
    pub midi_in: Option<String>,
}

pub fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(1);
    }

    let mut a = Args::default();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--plugin" => {
                i += 1;
                a.plugin_path = argv.get(i).cloned();
            }
            "--id" => {
                i += 1;
                a.plugin_id = argv.get(i).cloned();
            }
            "--output-device" => {
                i += 1;
                a.output_device = argv.get(i).cloned();
            }
            "--midi-in" => {
                i += 1;
                a.midi_in = argv.get(i).cloned();
            }
            "--set" => {
                i += 1;
                if let Some(kv) = argv.get(i).and_then(|s| parse_set(s)) {
                    a.sets.push(kv);
                } else {
                    eprintln!(
                        "error: --set wants <param_id>=<value>, got {:?}",
                        argv.get(i)
                    );
                    std::process::exit(1);
                }
            }
            "--pull-preset" => {
                i += 1;
                a.pull_preset = argv.get(i).cloned();
            }
            "--out" => {
                i += 1;
                a.pull_out = argv.get(i).map(PathBuf::from);
            }
            "--list-params" => a.list_params = true,
            "--list-presets" => a.list_presets = true,
            "--play" => a.play = true,
            "--list-midi" => a.list_midi = true,
            arg => eprintln!("warn: unknown arg {arg}"),
        }
        i += 1;
    }
    a
}

/// `"3=0.5"` → `(3, 0.5)`.
fn parse_set(s: &str) -> Option<(u32, f64)> {
    let (id, val) = s.split_once('=')?;
    Some((id.trim().parse().ok()?, val.trim().parse().ok()?))
}
