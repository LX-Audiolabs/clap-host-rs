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
    // CLAP spec: CLAP_PATH adds extra search dirs (`;`-separated on
    // Windows, `:` on other platforms).
    if let Ok(var) = std::env::var("CLAP_PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        dirs.extend(
            var.split(sep)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
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
