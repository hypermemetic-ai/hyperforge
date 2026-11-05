//! Cabal (Haskell) build system detection and manifest parsing

use std::path::Path;

/// A dependency a `.cabal` file names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRef {
    pub name: String,
    pub version_req: Option<String>,
    pub is_path_dep: bool,
    pub path: Option<String>,
    pub is_dev: bool,
}

/// Check if a directory contains a .cabal file
pub fn is_cabal_project(path: &Path) -> bool {
    find_cabal_file(path).is_some()
}

/// Find the .cabal file in a directory
pub fn find_cabal_file(path: &Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(path).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("cabal") && p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Parse the package name from a .cabal file
pub fn cabal_package_name(path: &Path) -> Option<String> {
    let cabal_path = find_cabal_file(path)?;
    let content = std::fs::read_to_string(cabal_path).ok()?;
    parse_cabal_field(&content, "name")
}

/// Parse the package version from a .cabal file
pub fn cabal_package_version(path: &Path) -> Option<String> {
    let cabal_path = find_cabal_file(path)?;
    let content = std::fs::read_to_string(cabal_path).ok()?;
    parse_cabal_field(&content, "version")
}

/// Parse a top-level field from cabal file content
fn parse_cabal_field(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.to_lowercase().starts_with(&prefix) {
            let value = trimmed[prefix.len()..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse dependencies from a .cabal file
///
/// Looks for `build-depends:` sections and parses package names
/// and version constraints. This is a simplified parser that handles
/// the common cases.
pub fn parse_cabal_deps(path: &Path) -> Vec<DepRef> {
    let cabal_path = match find_cabal_file(path) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let content = match std::fs::read_to_string(&cabal_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    parse_build_depends(&content)
}

/// Parse build-depends sections from cabal file content
fn parse_build_depends(content: &str) -> Vec<DepRef> {
    let mut deps = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut in_build_depends = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect start of build-depends section
        if trimmed.to_lowercase().starts_with("build-depends:") {
            in_build_depends = true;
            let rest = trimmed["build-depends:".len()..].trim();
            if !rest.is_empty() {
                parse_dep_list(rest, &mut deps, &mut seen);
            }
            continue;
        }

        // If we're in a build-depends continuation (indented lines)
        if in_build_depends {
            if line.starts_with(' ') || line.starts_with('\t') {
                // Check if this is a new field (contains a colon not inside version constraint)
                if trimmed.contains(':') && !trimmed.starts_with(',') && !trimmed.starts_with("--") {
                    // Could be a new field like "hs-source-dirs:" - check if it looks like a field
                    let before_colon = trimmed.split(':').next().unwrap_or("");
                    if !before_colon.contains(',')
                        && !before_colon.contains('>')
                        && !before_colon.contains('<')
                        && !before_colon.contains('=')
                    {
                        in_build_depends = false;
                        continue;
                    }
                }
                parse_dep_list(trimmed, &mut deps, &mut seen);
            } else {
                in_build_depends = false;
            }
        }
    }

    deps
}

/// Parse a comma-separated dependency list
fn parse_dep_list(
    text: &str,
    deps: &mut Vec<DepRef>,
    seen: &mut std::collections::HashSet<String>,
) {
    // Remove leading comma if present
    let text = text.trim_start_matches(',').trim();

    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() || part.starts_with("--") {
            continue;
        }

        // Split on version constraint operators
        let (name, version) = split_cabal_dep(part);
        let name = name.trim().to_string();

        if name.is_empty() || name == "base" {
            continue; // Skip 'base' as it's the standard library
        }

        if seen.insert(name.clone()) {
            deps.push(DepRef {
                name,
                version_req: version,
                is_path_dep: false,
                path: None,
                is_dev: false,
            });
        }
    }
}

/// List files that would be included in a published package.
///
/// Cabal does not have a lightweight file-listing command, so this
/// returns `None`. Callers should fall back to directory-scoped diff.
pub const fn cabal_publishable_files(_path: &Path) -> Option<Vec<String>> {
    None
}

/// Split a cabal dependency into name and version constraint
fn split_cabal_dep(dep: &str) -> (&str, Option<String>) {
    // Find first version constraint operator
    for (i, c) in dep.char_indices() {
        if c == '>' || c == '<' || c == '=' || c == '^' {
            let name = dep[..i].trim();
            let version = dep[i..].trim().to_string();
            return (name, Some(version));
        }
    }
    (dep.trim(), None)
}

/// Does `version` fall in a `build-depends` range (`>= 0.6 && < 0.7`,
/// `^>= 1.2.3`, `== 1.2.*`, `||`, `-any`)? `None`: a range this reader does
/// not read (a version set `{…}`, an unknown operator).
#[must_use]
pub fn range_matches(range: &str, version: &str) -> Option<bool> {
    use std::cmp::Ordering;
    let cmp = |a: &str, b: &str| super::chain::compare_versions(a, b);
    let one = |c: &str| -> Option<bool> {
        let c = c.trim().trim_start_matches('(').trim_end_matches(')').trim();
        match c {
            "-any" | "" => return Some(true),
            "-none" => return Some(false),
            _ => {}
        }
        let op_len = c.find(|ch: char| ch.is_ascii_digit()).unwrap_or(c.len());
        let (op, v) = (c[..op_len].trim(), c[op_len..].trim());
        if v.is_empty() || v.contains(['{', ' ']) {
            return None;
        }
        if let Some(prefix) = v.strip_suffix(".*") {
            return match op {
                "==" => Some(version == prefix || version.starts_with(&format!("{prefix}."))),
                _ => None,
            };
        }
        let o = cmp(version, v)?;
        Some(match op {
            ">=" => o != Ordering::Less,
            ">" => o == Ordering::Greater,
            "<=" => o != Ordering::Greater,
            "<" => o == Ordering::Less,
            "==" => o == Ordering::Equal,
            // PVP: the major version is the first two components.
            "^>=" => {
                let mut parts: Vec<u64> = v.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
                parts.resize(2, 0);
                let upper = format!("{}.{}", parts[0], parts[1] + 1);
                o != Ordering::Less && cmp(version, &upper)? == Ordering::Less
            }
            _ => return None,
        })
    };
    let mut any = false;
    for disjunct in range.split("||") {
        let mut all = true;
        for c in disjunct.split("&&") {
            all &= one(c)?;
        }
        any |= all;
    }
    Some(any)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_read_as_cabal_reads_them() {
        assert_eq!(range_matches(">= 0.5 && < 0.6", "0.5.2.0"), Some(true));
        assert_eq!(range_matches(">= 0.5 && < 0.6", "0.6.0.0"), Some(false));
        assert_eq!(range_matches(">=4.0 && <5", "4.0.0"), Some(true));
        assert_eq!(range_matches("^>= 1.2.3", "1.2.9"), Some(true));
        assert_eq!(range_matches("^>= 1.2.3", "1.3"), Some(false));
        assert_eq!(range_matches("^>= 1.2.3", "1.2.2"), Some(false));
        assert_eq!(range_matches("== 1.2.*", "1.2.4.1"), Some(true));
        assert_eq!(range_matches("== 1.2.*", "1.20"), Some(false));
        assert_eq!(range_matches("< 1 || >= 2", "2.1"), Some(true));
        assert_eq!(range_matches("-any", "9"), Some(true));
        assert_eq!(range_matches("^>= { 1.2, 1.3 }", "1.2"), None);
    }
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_is_cabal_project() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_cabal_project(tmp.path()));

        fs::write(
            tmp.path().join("mypackage.cabal"),
            "name: mypackage\nversion: 0.1.0\n",
        )
        .unwrap();
        assert!(is_cabal_project(tmp.path()));
    }

    #[test]
    fn test_cabal_package_name() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("mypackage.cabal"),
            "name:           mypackage\nversion:        0.1.0\n",
        )
        .unwrap();
        assert_eq!(
            cabal_package_name(tmp.path()),
            Some("mypackage".to_string())
        );
    }

    #[test]
    fn test_parse_cabal_deps() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("test.cabal"),
            r"name:           test
version:        0.1.0

library
  build-depends:
    , aeson >=2.0
    , text ^>=2.0
    , bytestring
  hs-source-dirs: src

executable test-exe
  build-depends:
    , optparse-applicative >=0.16
",
        )
        .unwrap();

        let deps = parse_cabal_deps(tmp.path());
        assert!(deps.iter().any(|d| d.name == "aeson"));
        assert!(deps.iter().any(|d| d.name == "text"));
        assert!(deps.iter().any(|d| d.name == "bytestring"));
        assert!(deps
            .iter()
            .any(|d| d.name == "optparse-applicative"));

        let aeson = deps.iter().find(|d| d.name == "aeson").unwrap();
        assert_eq!(aeson.version_req, Some(">=2.0".to_string()));
        assert!(!aeson.is_path_dep);
    }

    #[test]
    fn test_parse_cabal_inline_deps() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("test.cabal"),
            "name: test\nversion: 0.1.0\n\nlibrary\n  build-depends: aeson >=2.0, text, containers\n",
        )
        .unwrap();

        let deps = parse_cabal_deps(tmp.path());
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().any(|d| d.name == "aeson"));
        assert!(deps.iter().any(|d| d.name == "text"));
        assert!(deps.iter().any(|d| d.name == "containers"));
    }
}
