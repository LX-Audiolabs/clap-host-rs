//! Interactive plugin picker: the bare `clap-host-rs` experience.
//!
//! Scans the standard CLAP dirs once, prints a numbered list, and lets the
//! user pick by number or narrow the list by typing text. No TUI library —
//! plain stdin/stdout, line-oriented. The pick returns (path, clap id) for
//! main to open in the GUI.

use std::io::Write;
use std::path::PathBuf;

use clap_host_core::loader::Loader;
use clap_host_core::scan;

/// One plugin row — a single plugin inside a .clap file. Files with several
/// plugins produce several rows, so a number always selects exactly one
/// plugin and no second prompt is needed.
struct Entry {
    path: PathBuf,
    name: String,
    id: String,
}

/// Case-insensitive substring match over name + id + path; the filter's
/// whitespace-separated words must all match somewhere (word-AND).
fn entry_matches(name: &str, id: &str, path: &str, filter: &str) -> bool {
    let hay = format!("{name}\n{id}\n{path}").to_lowercase();
    filter
        .split_whitespace()
        .all(|word| hay.contains(&word.to_lowercase()))
}

/// Scan + describe every plugin. Broken .clap files are skipped with a
/// warning — a picker must not die on one bad plugin. Loaders are dropped
/// here; main re-opens only the picked file.
fn collect_entries() -> Vec<Entry> {
    let mut out = Vec::new();
    for path in scan::scan() {
        let display = path.display().to_string();
        let Some(p) = path.to_str() else {
            eprintln!("warn: {display}: not valid UTF-8, skipped");
            continue;
        };
        let loader = match unsafe { Loader::open(p) } {
            Ok(l) => l,
            Err(e) => {
                eprintln!("warn: {display}: {e}");
                continue;
            }
        };
        for idx in 0..loader.plugin_count() {
            if let Some(d) = loader.descriptor(idx) {
                let name = unsafe { std::ffi::CStr::from_ptr(d.name) }
                    .to_string_lossy()
                    .into_owned();
                let id = unsafe { std::ffi::CStr::from_ptr(d.id) }
                    .to_string_lossy()
                    .into_owned();
                out.push(Entry {
                    path: path.clone(),
                    name,
                    id,
                });
            }
        }
    }
    out
}

/// Run the pick loop. Returns (plugin file path, clap id), or None when the
/// user quits (empty input / q), stdin hits EOF, or nothing was found.
/// Call only when stdin is a TTY — main enforces that.
pub fn pick() -> Option<(String, String)> {
    let entries = collect_entries();
    if entries.is_empty() {
        eprintln!("no CLAP plugins found in the standard search dirs");
        return None;
    }
    let mut filter = String::new();
    loop {
        let visible: Vec<&Entry> = entries
            .iter()
            .filter(|e| entry_matches(&e.name, &e.id, &e.path.to_string_lossy(), &filter))
            .collect();
        if visible.is_empty() {
            println!("(no match for {filter:?} — type new text to re-filter)");
        } else {
            for (i, e) in visible.iter().enumerate() {
                println!("[{i}] {}  id={}  ({})", e.name, e.id, e.path.display());
            }
        }
        print!("number to open in the GUI, text to filter, Enter/q to quit\n> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).ok()? == 0 {
            return None; // EOF — stdin closed
        }
        let input = line.trim();
        if input.is_empty() || input.eq_ignore_ascii_case("q") {
            return None;
        }
        // A number selects; anything else narrows the list.
        if let Ok(n) = input.parse::<usize>()
            && let Some(e) = visible.get(n)
        {
            return Some((e.path.to_string_lossy().into_owned(), e.id.clone()));
        }
        filter = input.to_lowercase();
    }
}

#[cfg(test)]
mod tests {
    use super::entry_matches;

    #[test]
    fn filter_matches_name_id_and_path() {
        let path = "C:/Program Files/Common/CLAP/Vital.clap";
        assert!(entry_matches("Vital", "com.vital", path, "vital"));
        assert!(entry_matches("Vital", "com.vital", path, "VITAL"));
        assert!(entry_matches("Surge XT", "com.surge", "C:/CLAP/Surge.clap", "surge clap"));
        assert!(!entry_matches("Vital", "com.vital", path, "surge"));
        assert!(!entry_matches("Vital", "com.vital", path, "vital surge"));
    }
}
