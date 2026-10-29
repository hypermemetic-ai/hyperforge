//! Unified manifest parsing for Cargo.toml, package.json, pyproject.toml.
//!
//! Each parser yields a `PackageManifest` with the fields BuildService
//! surfaces. Unknown / missing optional fields fall back to empty.

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum ManifestError {
    #[error("manifest file not found: {0}")]
    NotFound(String),
    #[error("manifest parse error in {file}: {message}")]
    ParseError { file: String, message: String },
    #[error("io error: {0}")]
    Io(String),
}

impl ManifestError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "manifest_not_found",
            Self::ParseError { .. } => "manifest_parse_error",
            Self::Io(_) => "io",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Dep {
    pub name: String,
    pub version: String,
    /// `cargo` | `npm` | `pypi` — which manifest kind this came from.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct PackageManifest {
    /// `cargo` | `npm` | `pypi`.
    pub kind: String,
    pub name: String,
    pub version: String,
    pub deps: Vec<Dep>,
}

/// Detect + parse the first manifest found in `dir`. Order of
/// preference: Cargo.toml, package.json, pyproject.toml.
pub fn detect_and_parse(dir: &Path) -> Result<PackageManifest, ManifestError> {
    let cargo = dir.join("Cargo.toml");
    if cargo.is_file() {
        return parse_cargo(&cargo);
    }
    let npm = dir.join("package.json");
    if npm.is_file() {
        return parse_npm(&npm);
    }
    let py = dir.join("pyproject.toml");
    if py.is_file() {
        return parse_pyproject(&py);
    }
    Err(ManifestError::NotFound(dir.display().to_string()))
}

pub fn parse_cargo(path: &Path) -> Result<PackageManifest, ManifestError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
    parse_cargo_str(&raw, path)
}

pub fn parse_cargo_str(raw: &str, path: &Path) -> Result<PackageManifest, ManifestError> {
    let v: toml::Value = toml::from_str(raw)
        .map_err(|e| ManifestError::ParseError { file: path.display().to_string(), message: e.to_string() })?;
    let pkg = v.get("package").and_then(|t| t.as_table())
        .ok_or_else(|| ManifestError::ParseError {
            file: path.display().to_string(),
            message: "missing [package]".into(),
        })?;
    let name = pkg.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let version = pkg.get("version").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let mut deps: Vec<Dep> = Vec::new();
    if let Some(table) = v.get("dependencies").and_then(|t| t.as_table()) {
        for (k, val) in table {
            deps.push(Dep {
                name: k.clone(),
                version: dep_version(val),
                source: "cargo".into(),
            });
        }
    }
    Ok(PackageManifest { kind: "cargo".into(), name, version, deps })
}

fn dep_version(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => t.get("version").and_then(|s| s.as_str()).unwrap_or("*").to_string(),
        _ => "*".into(),
    }
}

pub fn parse_npm(path: &Path) -> Result<PackageManifest, ManifestError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ManifestError::ParseError { file: path.display().to_string(), message: e.to_string() })?;
    let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let version = v.get("version").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let mut deps: Vec<Dep> = Vec::new();
    for field in ["dependencies", "devDependencies"] {
        if let Some(obj) = v.get(field).and_then(|o| o.as_object()) {
            for (k, val) in obj {
                deps.push(Dep {
                    name: k.clone(),
                    version: val.as_str().unwrap_or("*").to_string(),
                    source: "npm".into(),
                });
            }
        }
    }
    Ok(PackageManifest { kind: "npm".into(), name, version, deps })
}

pub fn parse_pyproject(path: &Path) -> Result<PackageManifest, ManifestError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
    let v: toml::Value = toml::from_str(&raw)
        .map_err(|e| ManifestError::ParseError { file: path.display().to_string(), message: e.to_string() })?;
    // PEP 621: [project] block; fallback to [tool.poetry].
    let (name, version, deps_list) =
        if let Some(project) = v.get("project").and_then(|t| t.as_table()) {
            let n = project.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let ver = project.get("version").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let deps = project.get("dependencies")
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default();
            (n, ver, deps)
        } else if let Some(p) = v.get("tool").and_then(|t| t.get("poetry")).and_then(|t| t.as_table()) {
            let n = p.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let ver = p.get("version").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let deps_tbl = p.get("dependencies").and_then(|t| t.as_table());
            let deps: Vec<String> = if let Some(d) = deps_tbl {
                d.iter().map(|(k, val)| format!("{}=={}", k, val.as_str().unwrap_or("*"))).collect()
            } else { Vec::new() };
            (n, ver, deps)
        } else {
            return Err(ManifestError::ParseError {
                file: path.display().to_string(),
                message: "missing [project] or [tool.poetry]".into(),
            });
        };
    let deps: Vec<Dep> = deps_list.into_iter().map(|spec| {
        // Split "name==ver" or "name>=ver" best-effort; default to spec as name.
        let (n, v) = match spec.split_once("==")
            .or_else(|| spec.split_once(">="))
            .or_else(|| spec.split_once("~="))
            .or_else(|| spec.split_once('>')) {
            Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
            None => (spec.trim().to_string(), "*".to_string()),
        };
        Dep { name: n, version: v, source: "pypi".into() }
    }).collect();
    Ok(PackageManifest { kind: "pypi".into(), name, version, deps })
}

/// Analyze a set of manifests for cross-repo anomalies. Returns a list
/// of findings; callers decide how to emit them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// Two manifests declare the same package name with different versions.
    DuplicateName { name: String, versions: Vec<String> },
    /// Two manifests declare a shared dependency at different versions.
    VersionMismatch { dep: String, versions: Vec<String> },
}

pub fn analyze(manifests: &[PackageManifest]) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    // Duplicate names.
    let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in manifests {
        if m.name.is_empty() { continue; }
        by_name.entry(&m.name).or_default().push(&m.version);
    }
    for (name, versions) in by_name {
        let unique: std::collections::BTreeSet<&&str> = versions.iter().collect();
        if versions.len() > 1 && unique.len() > 1 {
            findings.push(Finding::DuplicateName {
                name: name.to_string(),
                versions: versions.into_iter().map(String::from).collect(),
            });
        }
    }
    // Cross-manifest dep version mismatches.
    let mut deps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in manifests {
        for d in &m.deps {
            deps.entry(&d.name).or_default().push(&d.version);
        }
    }
    for (dep, versions) in deps {
        let unique: std::collections::BTreeSet<&&str> = versions.iter().collect();
        if unique.len() > 1 {
            findings.push(Finding::VersionMismatch {
                dep: dep.to_string(),
                versions: unique.into_iter().map(|s| (*s).to_string()).collect(),
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_cargo_ok() {
        let raw = r#"
            [package]
            name = "foo"
            version = "0.1.2"
            [dependencies]
            serde = "1.0"
            tokio = { version = "1.30" }
        "#;
        let m = parse_cargo_str(raw, &PathBuf::from("<mem>")).unwrap();
        assert_eq!(m.name, "foo");
        assert_eq!(m.version, "0.1.2");
        assert_eq!(m.deps.len(), 2);
    }

    #[test]
    fn analyze_spots_version_mismatch() {
        let a = PackageManifest {
            kind: "cargo".into(), name: "a".into(), version: "0.1.0".into(),
            deps: vec![Dep { name: "serde".into(), version: "1.0.200".into(), source: "cargo".into() }],
        };
        let b = PackageManifest {
            kind: "cargo".into(), name: "b".into(), version: "0.1.0".into(),
            deps: vec![Dep { name: "serde".into(), version: "1.0.150".into(), source: "cargo".into() }],
        };
        let findings = analyze(&[a, b]);
        assert!(findings.iter().any(|f| matches!(f, Finding::VersionMismatch { dep, .. } if dep == "serde")));
    }
}
