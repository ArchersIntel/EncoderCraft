//! Directory listing shared by the daemon (restricted to media roots) and the desktop app.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Listings are capped so a folder with millions of files can't stall the UI or the API.
pub const MAX_ENTRIES: usize = 5000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirListing {
    /// Canonical path that was listed.
    pub path: String,
    /// Folders first, then files, each alphabetical (case-insensitive).
    pub entries: Vec<DirEntry>,
    /// More than `MAX_ENTRIES` entries existed.
    pub truncated: bool,
}

/// Windows canonical paths carry a `\\?\` prefix that is noise for users (and for file names we
/// hand back to them). `\\?\C:\x` becomes `C:\x`, `\\?\UNC\srv\share` becomes `\\srv\share`.
pub fn plain_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => s.into_owned(),
    }
}

/// Lists `path` without following symlinks for entry metadata. Hidden dot-files are skipped.
pub fn list_dir(path: &Path) -> std::io::Result<DirListing> {
    let canonical = path.canonicalize()?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(&canonical)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        // Follow symlinks only to classify (so a link to a folder shows as a folder).
        let meta = std::fs::metadata(entry.path()).or_else(|_| entry.metadata());
        let Ok(meta) = meta else { continue };
        entries.push(DirEntry { name, is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() } });
    }
    entries.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
    Ok(DirListing { path: plain_path(&canonical), entries, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_windows_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Users\a")), r"C:\Users\a");
        assert_eq!(plain_path(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
        assert_eq!(plain_path(Path::new("/mnt/nas")), "/mnt/nas");
        assert_eq!(plain_path(Path::new(r"C:\plain")), r"C:\plain");
    }

    #[test]
    fn folders_first_sorted_and_hidden_skipped() {
        let d = std::env::temp_dir().join(format!("encraft-fs-{}", std::process::id()));
        std::fs::create_dir_all(d.join("Zeta")).unwrap();
        std::fs::create_dir_all(d.join("alpha")).unwrap();
        std::fs::write(d.join("b.mov"), b"12345").unwrap();
        std::fs::write(d.join("A.mov"), b"1").unwrap();
        std::fs::write(d.join(".hidden"), b"x").unwrap();
        let l = list_dir(&d).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "A.mov", "b.mov"]);
        assert_eq!(l.entries[3].size, 5);
        assert!(!l.truncated);
        let _ = std::fs::remove_dir_all(d);
    }
}
