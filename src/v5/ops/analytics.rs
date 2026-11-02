//! `ops::analytics` — read-only filesystem analytics over a checkout
//! directory (V5PARITY-4). Pure `std::fs` walks; no subprocess, no git.
//!
//! Dirty-check lives in `ops::git` (shared with V5PARITY-3); the service
//! routes `repos.dirty` through that single implementation. D13.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum AnalyticsError {
    #[error("path is not a directory: {0}")]
    NotADir(String),
    #[error("io error: {0}")]
    Io(String),
}

impl AnalyticsError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotADir(_) => "not_a_directory",
            Self::Io(_) => "io",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeSummary {
    pub bytes: u64,
    pub file_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    pub path: String,
    pub size: u64,
}

/// Walk `dir` and count total file bytes + file count. Skips `.git/`.
pub fn repo_size(dir: &Path) -> Result<SizeSummary, AnalyticsError> {
    if !dir.is_dir() {
        return Err(AnalyticsError::NotADir(dir.display().to_string()));
    }
    let mut summary = SizeSummary { bytes: 0, file_count: 0 };
    walk(dir, &mut |entry| {
        summary.bytes += entry.size;
        summary.file_count += 1;
    })?;
    Ok(summary)
}

/// Line counts per detected language, keyed by a short language tag.
/// Files with no recognised extension fall into `"other"`.
pub fn repo_loc(dir: &Path) -> Result<BTreeMap<String, u64>, AnalyticsError> {
    if !dir.is_dir() {
        return Err(AnalyticsError::NotADir(dir.display().to_string()));
    }
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    walk(dir, &mut |entry| {
        let Some(lang) = classify(&entry.path) else { return };
        let Ok(content) = std::fs::read_to_string(&entry.path) else { return };
        let lines = content.lines().count() as u64;
        *out.entry(lang.to_string()).or_insert(0) += lines;
    })?;
    Ok(out)
}

/// Files at or above `threshold_bytes`. Sorted by descending size.
pub fn large_files(dir: &Path, threshold_bytes: u64) -> Result<Vec<LargeFile>, AnalyticsError> {
    if !dir.is_dir() {
        return Err(AnalyticsError::NotADir(dir.display().to_string()));
    }
    let mut out: Vec<LargeFile> = Vec::new();
    walk(dir, &mut |entry| {
        if entry.size >= threshold_bytes {
            out.push(LargeFile {
                path: entry.rel.clone(),
                size: entry.size,
            });
        }
    })?;
    out.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(out)
}

// ---------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------

struct WalkEntry {
    /// Absolute (to open/read) path.
    path: PathBuf,
    /// Path relative to the walk root (for reporting).
    rel: String,
    size: u64,
}

/// Recursive directory walk, skipping `.git/`. Follows symlinks only
/// as file metadata (not directories) to avoid cycles.
fn walk(
    root: &Path,
    visit: &mut dyn FnMut(&WalkEntry),
) -> Result<(), AnalyticsError> {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let rd = std::fs::read_dir(&cur).map_err(|e| AnalyticsError::Io(e.to_string()))?;
        for ent in rd.flatten() {
            let p = ent.path();
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if p.file_name().and_then(|s| s.to_str()) == Some(".git") {
                    continue;
                }
                stack.push(p);
            } else if ft.is_file() {
                let meta = match ent.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let size = meta.len();
                let rel = p.strip_prefix(root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| p.display().to_string());
                visit(&WalkEntry { path: p, rel, size });
            }
        }
    }
    Ok(())
}

/// Map a file path to a short language tag via its extension.
/// Unknown extensions return `None`; the caller treats them as "other".
fn classify(path: &Path) -> Option<&'static str> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    Some(match ext {
        "rs" => "rust",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" => "markdown",
        "sh" | "bash" => "shell",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "css" => "css",
        "html" | "htm" => "html",
        "lock" => "lockfile",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mktmp() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("v5-analytics-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn size_skips_dotgit() {
        let d = mktmp();
        std::fs::write(d.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join(".git").join("ignored.txt"), b"nope").unwrap();
        let s = repo_size(&d).unwrap();
        assert_eq!(s.file_count, 1);
        assert_eq!(s.bytes, 5);
    }

    #[test]
    fn loc_groups_by_extension() {
        let d = mktmp();
        std::fs::write(d.join("a.rs"), "fn main() {}\n// line\n").unwrap();
        std::fs::write(d.join("b.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(d.join("c.toml"), "[a]\nb = 1\n").unwrap();
        let m = repo_loc(&d).unwrap();
        assert_eq!(m.get("rust"), Some(&3));
        assert_eq!(m.get("toml"), Some(&2));
    }

    #[test]
    fn large_files_filters_and_sorts() {
        let d = mktmp();
        std::fs::write(d.join("tiny"), &[0u8; 10]).unwrap();
        std::fs::write(d.join("big"), &[0u8; 1024]).unwrap();
        std::fs::write(d.join("huge"), &[0u8; 4096]).unwrap();
        let out = large_files(&d, 1000).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].size, 4096);
        assert_eq!(out[1].size, 1024);
    }
}
