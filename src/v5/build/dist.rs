//! `build::dist` — distribution channel scaffolding:
//! dist.toml template, cargo-binstall stanza, Homebrew formula.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum DistError {
    #[error("io error: {0}")]
    Io(String),
    #[error("manifest parse error: {0}")]
    Toml(String),
    #[error("manifest file not found: {0}")]
    NotFound(String),
}

impl DistError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Toml(_) => "manifest_parse_error",
            Self::NotFound(_) => "not_found",
        }
    }
}

const DEFAULT_DIST_TOML: &str = r#"# Hyperforge dist.toml — distribution metadata (V5PARITY-11).
#
# This file is separate from `.hyperforge/config.toml` (per-repo
# identity) to keep distribution concerns cleanly split.

[dist]
# Channels this package publishes to. Supported: crates.io, npm, pypi,
# hackage, homebrew, cargo-binstall.
channels = []

[binary]
# Name of the shipped binary(ies), if any.
names = []
"#;

/// Write `.hyperforge/dist.toml` if missing. Returns `(path, created)`.
/// A pre-existing file is left untouched (idempotent).
pub fn init_dist_toml(repo_dir: &Path) -> Result<(PathBuf, bool), DistError> {
    let hf_dir = repo_dir.join(".hyperforge");
    std::fs::create_dir_all(&hf_dir).map_err(|e| DistError::Io(e.to_string()))?;
    let path = hf_dir.join("dist.toml");
    if path.exists() {
        return Ok((path, false));
    }
    std::fs::write(&path, DEFAULT_DIST_TOML).map_err(|e| DistError::Io(e.to_string()))?;
    Ok((path, true))
}

/// Read an existing dist.toml. Returns `None` if absent.
pub fn read_dist_toml(repo_dir: &Path) -> Result<Option<String>, DistError> {
    let path = repo_dir.join(".hyperforge").join("dist.toml");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path).map_err(|e| DistError::Io(e.to_string()))?))
}

/// Ensure a `[package.metadata.binstall]` stanza exists in `Cargo.toml`.
/// Preserves formatting via `toml_edit`. Returns `true` if the file was
/// modified (was missing the stanza), `false` if it was already present.
pub fn binstall_init(cargo_toml: &Path) -> Result<bool, DistError> {
    if !cargo_toml.is_file() {
        return Err(DistError::NotFound(cargo_toml.display().to_string()));
    }
    let raw = std::fs::read_to_string(cargo_toml).map_err(|e| DistError::Io(e.to_string()))?;
    let mut doc: toml_edit::DocumentMut = raw.parse()
        .map_err(|e: toml_edit::TomlError| DistError::Toml(e.to_string()))?;
    // Walk to `package.metadata.binstall`; create intermediate tables.
    let pkg = doc.entry("package").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let pkg_tbl = pkg.as_table_mut().ok_or_else(|| DistError::Toml("[package] is not a table".into()))?;
    let md = pkg_tbl.entry("metadata").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let md_tbl = md.as_table_mut().ok_or_else(|| DistError::Toml("[package.metadata] is not a table".into()))?;
    if md_tbl.contains_key("binstall") {
        return Ok(false);
    }
    let mut binstall = toml_edit::Table::new();
    binstall.set_implicit(false);
    binstall.insert(
        "pkg-url",
        toml_edit::value(
            "{ repo }/releases/download/v{ version }/{ name }-{ target }{ archive-suffix }",
        ),
    );
    binstall.insert("pkg-fmt", toml_edit::value("tgz"));
    md_tbl.insert("binstall", toml_edit::Item::Table(binstall));
    std::fs::write(cargo_toml, doc.to_string()).map_err(|e| DistError::Io(e.to_string()))?;
    Ok(true)
}

/// Generate a Homebrew formula text for `name` at `version`,
/// shipping `tarball_url` with sha256 `sha`.
#[must_use]
pub fn brew_formula(name: &str, version: &str, tarball_url: &str, sha256: &str, desc: &str) -> String {
    // Cap first char.
    let class_name: String = name.split(&['-', '_'][..])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
            }
        })
        .collect();
    format!(
        r#"class {class_name} < Formula
  desc "{desc}"
  homepage ""
  url "{tarball_url}"
  sha256 "{sha256}"
  version "{version}"

  def install
    bin.install "{name}"
  end
end
"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brew_class_name_camel() {
        let f = brew_formula("my-tool", "1.2.3", "http://x", "deadbeef", "hello");
        assert!(f.starts_with("class MyTool < Formula"));
    }

    #[test]
    fn binstall_init_idempotent() {
        let dir = std::env::temp_dir().join(format!("binstall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cargo = dir.join("Cargo.toml");
        std::fs::write(&cargo, "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n").unwrap();
        assert!(binstall_init(&cargo).unwrap());
        assert!(!binstall_init(&cargo).unwrap());
    }
}
