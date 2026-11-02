//! YAML config loaders for the three v5 schemas (V5CORE-3).
//!
//! * Global `config.yaml`
//! * Per-org `orgs/<OrgName>.yaml`
//! * Per-workspace `workspaces/<WorkspaceName>.yaml`
//!
//! All types come from CONTRACTS §types. Unknown top-level fields,
//! unknown enum variants, and basename/name mismatches are hard errors.
//! Writes are atomic (temp + rename) per D8.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------
// §types — newtypes and enums referenced by the composite schemas.
// ---------------------------------------------------------------------

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            Hash,
            Serialize,
            Deserialize,
            PartialOrd,
            Ord,
            JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(OrgName);
string_newtype!(RepoName);
string_newtype!(WorkspaceName);
string_newtype!(RemoteUrl);
string_newtype!(DomainName);
string_newtype!(FsPath);

/// Forge providers known to v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Github,
    Codeberg,
    Gitlab,
}

/// Credential kinds known to v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    Token,
    SshKey,
}

/// `CredentialEntry { key, type }`. `key` is either a `secrets://…` ref
/// or an `FsPath`; both serialize as a bare string, so the wire form is
/// `{key: "...", type: "token"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialEntry {
    pub key: String,
    #[serde(rename = "type")]
    pub cred_type: CredentialType,
}

/// Remote URL, optionally overriding the domain → provider map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub url: RemoteUrl,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<ProviderKind>,
}

/// `RepoRef { org, name }`.
///
/// Serialized canonically as a JSON object `{org, name}`. For YAML
/// convenience, `Deserialize` also accepts the human-friendly string
/// form `<org>/<name>` — used inside workspace yaml `ref:` fields and
/// on-the-wire wherever callers find the object form redundant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    pub org: OrgName,
    pub name: RepoName,
}

impl<'de> Deserialize<'de> for RepoRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept either the canonical object `{org, name}` or the
        // shorthand string `<org>/<name>`.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Object { org: OrgName, name: RepoName },
            String(String),
        }
        let v = Either::deserialize(deserializer)?;
        match v {
            Either::Object { org, name } => Ok(Self { org, name }),
            Either::String(s) => {
                let (org, name) = s.split_once('/').ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "invalid RepoRef shorthand (expected '<org>/<name>'): {s}"
                    ))
                })?;
                if org.is_empty() || name.is_empty() {
                    return Err(serde::de::Error::custom(format!(
                        "invalid RepoRef shorthand (empty segment): {s}"
                    )));
                }
                Ok(Self {
                    org: org.into(),
                    name: name.into(),
                })
            }
        }
    }
}

/// A repo declared on an org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgRepo {
    pub name: RepoName,
    #[serde(default)]
    pub remotes: Vec<Remote>,
    /// V5PARITY-34: when present, scopes which forges this repo lives
    /// on. Routing ops (sync, push, set_archived, …) intersect this
    /// list with the providers derived from `remotes`. `None` =
    /// unscoped; every remote's provider participates (legacy
    /// behavior, byte-identical for fixtures that don't carry the
    /// field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forges: Option<Vec<ProviderKind>>,
    /// Optional declared-local values for the D3 portable metadata set.
    /// When absent, `repos.sync` treats the local side as "unknown" for
    /// that field and reports drift only against the declared keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RepoMetadataLocal>,
}

/// Local declaration of portable metadata. Fields are optional; only
/// declared fields participate in drift comparisons / pushes.
///
/// V5LIFECYCLE-5 added `lifecycle`, `privatized_on`, `protected`.
/// These default such that an absent `metadata:` block OR a metadata
/// block without the new fields round-trips byte-identically after a
/// load → save cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoMetadataLocal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// `active` (normal) | `dismissed` (soft-deleted). `purged` is a
    /// transient — a purged repo is removed from the org yaml. The
    /// default is `Active`; serialized as absent so repos that never
    /// went through soft-delete stay byte-identical on round-trip.
    #[serde(default, skip_serializing_if = "RepoLifecycle::is_default")]
    pub lifecycle: RepoLifecycle,
    /// Providers where privatization succeeded during soft-delete.
    /// Empty set → serialized as absent (skip_serializing_if).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub privatized_on: std::collections::BTreeSet<ProviderKind>,
    /// When true, `repos.delete` and `repos.purge` refuse this repo.
    /// Default false is serialized as absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub protected: bool,
}

/// Lifecycle phases a repo can occupy. See D12. `Active` is the
/// default — V5PARITY-12 upgraded this from `Option<RepoLifecycle>` to
/// a required field so "unset vs Active" is no longer representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoLifecycle {
    #[default]
    Active,
    Dismissed,
}

impl RepoLifecycle {
    /// Used by `RepoMetadataLocal` via `skip_serializing_if` to keep
    /// `Active` (the default) absent on the wire.
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Active)
    }
}

impl OrgRepo {
    /// The canonical / primary remote — by position, `remotes[0]`.
    /// v5 doesn't model primary/mirror as a type-level split; the
    /// position convention is enforced via this helper so the intent
    /// is visible at call sites.
    #[must_use]
    pub fn canonical_remote(&self) -> Option<&Remote> {
        self.remotes.first()
    }
}

/// A workspace entry, either a string shorthand `<org>/<name>` or a
/// `{ref, dir}` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum WorkspaceRepo {
    Shorthand(String),
    Object {
        #[serde(rename = "ref")]
        reference: RepoRef,
        dir: String,
    },
}

// ---------------------------------------------------------------------
// Schema roots.
// ---------------------------------------------------------------------

/// Global `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<WorkspaceName>,
    #[serde(default)]
    pub provider_map: BTreeMap<DomainName, ProviderKind>,
    /// Owner-alias identity (HYPE-5). Maps a **canonical** owner to the
    /// list of alias owners that name the same identity — e.g.
    /// `hypermemetic-ai: [hypermemetic]` (the org is canonical; the old
    /// user path is an alias). When a repo moves user→org, GitHub
    /// live-redirects the old owner, so the two strings are one identity;
    /// this table lets every owner **comparison** agree on that without
    /// rewriting any URL. Aliases are curated/directional — two owners
    /// are equal only when the table says so, so a genuine unrelated
    /// mismatch is still caught. Resolved through
    /// [`GlobalConfig::canonical_owner`].
    #[serde(default)]
    pub owner_aliases: BTreeMap<OrgName, Vec<OrgName>>,
}

impl GlobalConfig {
    /// Resolve `owner` to its canonical form through [`Self::owner_aliases`]
    /// (HYPE-5). Identity when `owner` is already canonical or is unknown to
    /// the table — so non-aliased owners are unchanged. This is the single
    /// source of truth for owner-identity **comparison and display**; it
    /// never rewrites URLs or `.git/config` and never performs migration.
    #[must_use]
    pub fn canonical_owner(&self, owner: &str) -> String {
        for (canonical, aliases) in &self.owner_aliases {
            // Forge owners are case-insensitive (GitHub `JuggernautLabs`
            // is `juggernautlabs`): HYPERFORGE-58.
            if canonical.as_str().eq_ignore_ascii_case(owner)
                || aliases.iter().any(|a| a.as_str().eq_ignore_ascii_case(owner))
            {
                return canonical.0.clone();
            }
        }
        owner.to_string()
    }

    /// True when two owner strings name the same canonical identity
    /// (HYPE-5). Use this instead of `a == b` at every owner-comparison
    /// site so aliased owners compare equal and unrelated owners do not.
    #[must_use]
    pub fn same_owner(&self, a: &str, b: &str) -> bool {
        self.canonical_owner(a).eq_ignore_ascii_case(&self.canonical_owner(b))
    }

    /// Flatten the alias table to `alias -> canonical` pairs (HYPE-5).
    /// Used to render the alias-aware pre-push hook, whose embedded
    /// `_canon()` mirrors [`Self::canonical_owner`].
    #[must_use]
    pub fn owner_alias_pairs(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (canonical, aliases) in &self.owner_aliases {
            for a in aliases {
                out.insert(a.0.clone(), canonical.0.clone());
            }
        }
        out
    }
}

/// An org's forge block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeBlock {
    pub provider: ProviderKind,
    #[serde(default)]
    pub credentials: Vec<CredentialEntry>,
}

/// Per-org `orgs/<name>.yaml` schema root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgConfig {
    pub name: OrgName,
    pub forge: ForgeBlock,
    /// HYPERFORGE-60: further forges the org's repos live on, each with its
    /// own credentials. A repo's registered remotes on these providers are
    /// mirror targets ().
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<ForgeBlock>,
    #[serde(default)]
    pub repos: Vec<OrgRepo>,
}

/// Per-workspace `workspaces/<name>.yaml` schema root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: WorkspaceName,
    pub path: FsPath,
    #[serde(default)]
    pub repos: Vec<WorkspaceRepo>,
}

// ---------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid yaml in {file}: {source}")]
    InvalidYaml {
        file: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error(
        "basename/name mismatch in orgs/{file}: expected 'name: {file}' but got 'name: {found}'"
    )]
    OrgNameMismatch { file: String, found: String },
    #[error(
        "basename/name mismatch in workspaces/{file}: expected 'name: {file}' but got 'name: {found}'"
    )]
    WorkspaceNameMismatch { file: String, found: String },
    #[error("I/O error for {file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------
// Loaders.
// ---------------------------------------------------------------------

/// Everything a v5 daemon knows about on-disk state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadedConfig {
    pub global: GlobalConfig,
    /// Orgs keyed by name for O(1) lookup.
    pub orgs: BTreeMap<OrgName, OrgConfig>,
    pub workspaces: BTreeMap<WorkspaceName, WorkspaceConfig>,
}

/// Load every YAML file under `config_dir` into a typed bundle.
/// Missing `config.yaml` → default empty config. Missing `orgs/` or
/// `workspaces/` → empty maps. All other errors propagate.
pub fn load_all(config_dir: &Path) -> Result<LoadedConfig, ConfigError> {
    let global = load_global(&config_dir.join("config.yaml"))?;

    let orgs_dir = config_dir.join("orgs");
    let orgs = if orgs_dir.is_dir() {
        load_orgs(&orgs_dir)?
    } else {
        BTreeMap::new()
    };

    let ws_dir = config_dir.join("workspaces");
    let workspaces = if ws_dir.is_dir() {
        load_workspaces(&ws_dir)?
    } else {
        BTreeMap::new()
    };

    Ok(LoadedConfig {
        global,
        orgs,
        workspaces,
    })
}

/// Load `config.yaml`. Missing → default empty config.
pub fn load_global(path: &Path) -> Result<GlobalConfig, ConfigError> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GlobalConfig::default()),
        Err(e) => {
            return Err(ConfigError::Io {
                file: path.display().to_string(),
                source: e,
            });
        }
    };
    // serde_yaml silently returns `Null` for an empty document; normalise.
    if raw.trim().is_empty() {
        return Ok(GlobalConfig::default());
    }
    serde_yaml::from_str(&raw).map_err(|e| ConfigError::InvalidYaml {
        file: path.display().to_string(),
        source: e,
    })
}

/// Load every `orgs/*.yaml` in the directory.
pub fn load_orgs(dir: &Path) -> Result<BTreeMap<OrgName, OrgConfig>, ConfigError> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).map_err(|e| ConfigError::Io {
        file: dir.display().to_string(),
        source: e,
    })? {
        let entry = entry.map_err(|e| ConfigError::Io {
            file: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let basename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let raw = fs::read_to_string(&path).map_err(|e| ConfigError::Io {
            file: path.display().to_string(),
            source: e,
        })?;
        let cfg: OrgConfig = serde_yaml::from_str(&raw).map_err(|e| ConfigError::InvalidYaml {
            file: path.display().to_string(),
            source: e,
        })?;
        if cfg.name.as_str() != basename {
            return Err(ConfigError::OrgNameMismatch {
                file: basename,
                found: cfg.name.0,
            });
        }
        out.insert(cfg.name.clone(), cfg);
    }
    Ok(out)
}

/// Load every `workspaces/*.yaml` in the directory.
pub fn load_workspaces(dir: &Path) -> Result<BTreeMap<WorkspaceName, WorkspaceConfig>, ConfigError> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).map_err(|e| ConfigError::Io {
        file: dir.display().to_string(),
        source: e,
    })? {
        let entry = entry.map_err(|e| ConfigError::Io {
            file: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let basename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let raw = fs::read_to_string(&path).map_err(|e| ConfigError::Io {
            file: path.display().to_string(),
            source: e,
        })?;
        let cfg: WorkspaceConfig =
            serde_yaml::from_str(&raw).map_err(|e| ConfigError::InvalidYaml {
                file: path.display().to_string(),
                source: e,
            })?;
        if cfg.name.as_str() != basename {
            return Err(ConfigError::WorkspaceNameMismatch {
                file: basename,
                found: cfg.name.0,
            });
        }
        out.insert(cfg.name.clone(), cfg);
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Writers — atomic (temp + rename) per D8.
// ---------------------------------------------------------------------

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConfigError::Io {
            file: parent.display().to_string(),
            source: e,
        })?;
    }
    let tmp = tempfile_in(path.parent().unwrap_or(Path::new(".")))?;
    fs::write(&tmp, bytes).map_err(|e| ConfigError::Io {
        file: tmp.display().to_string(),
        source: e,
    })?;
    fs::rename(&tmp, path).map_err(|e| ConfigError::Io {
        file: path.display().to_string(),
        source: e,
    })
}

fn tempfile_in(dir: &Path) -> Result<PathBuf, ConfigError> {
    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for _ in 0..32 {
        let name = format!(".v5-atomic-{rng:x}");
        let p = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(_) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                rng = rng.wrapping_add(1);
            }
            Err(e) => {
                return Err(ConfigError::Io {
                    file: p.display().to_string(),
                    source: e,
                });
            }
        }
    }
    Err(ConfigError::Io {
        file: dir.display().to_string(),
        source: std::io::Error::other("failed to allocate temp file after 32 tries"),
    })
}

/// Write `config.yaml` atomically.
pub fn save_global(path: &Path, cfg: &GlobalConfig) -> Result<(), ConfigError> {
    let bytes = serde_yaml::to_string(cfg)
        .map_err(|e| ConfigError::InvalidYaml {
            file: path.display().to_string(),
            source: e,
        })?
        .into_bytes();
    write_atomic(path, &bytes)
}

/// Write `orgs/<name>.yaml` atomically.
pub fn save_org(dir: &Path, cfg: &OrgConfig) -> Result<(), ConfigError> {
    let path = dir.join(format!("{}.yaml", cfg.name));
    let bytes = serde_yaml::to_string(cfg)
        .map_err(|e| ConfigError::InvalidYaml {
            file: path.display().to_string(),
            source: e,
        })?
        .into_bytes();
    write_atomic(&path, &bytes)
}

/// Write `workspaces/<name>.yaml` atomically.
pub fn save_workspace(dir: &Path, cfg: &WorkspaceConfig) -> Result<(), ConfigError> {
    let path = dir.join(format!("{}.yaml", cfg.name));
    let bytes = serde_yaml::to_string(cfg)
        .map_err(|e| ConfigError::InvalidYaml {
            file: path.display().to_string(),
            source: e,
        })?
        .into_bytes();
    write_atomic(&path, &bytes)
}

// ---------------------------------------------------------------------
// Tests — exercise round-trip, error cases, and fixtures.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_dir(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("v5")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn empty_fixture_loads() {
        let cfg = load_all(&fixture_dir("empty")).expect("empty fixture loads");
        assert!(cfg.global.default_workspace.is_none());
        assert!(cfg.global.provider_map.is_empty());
        assert!(cfg.orgs.is_empty());
        assert!(cfg.workspaces.is_empty());
    }

    // ── HYPE-5: owner-alias identity ────────────────────────────────
    fn aliased() -> GlobalConfig {
        let mut owner_aliases = BTreeMap::new();
        owner_aliases.insert(
            OrgName::from("hypermemetic-ai"),
            vec![OrgName::from("hypermemetic")],
        );
        GlobalConfig {
            owner_aliases,
            ..Default::default()
        }
    }

    #[test]
    fn canonical_owner_resolves_alias_and_is_identity_otherwise() {
        let g = aliased();
        // alias -> canonical; canonical -> itself; the two agree.
        assert_eq!(g.canonical_owner("hypermemetic"), "hypermemetic-ai");
        assert_eq!(g.canonical_owner("hypermemetic-ai"), "hypermemetic-ai");
        assert_eq!(
            g.canonical_owner("hypermemetic"),
            g.canonical_owner("hypermemetic-ai")
        );
        assert!(g.same_owner("hypermemetic", "hypermemetic-ai"));
        // an unrelated owner is unchanged and is NOT the alias identity.
        assert_eq!(g.canonical_owner("someone-else"), "someone-else");
        assert!(!g.same_owner("someone-else", "hypermemetic-ai"));
        // with no aliases, canonical_owner is pure identity.
        let plain = GlobalConfig::default();
        assert_eq!(plain.canonical_owner("hypermemetic"), "hypermemetic");
        assert!(!plain.same_owner("hypermemetic", "hypermemetic-ai"));
    }

    #[test]
    fn owner_alias_pairs_flattens_alias_to_canonical() {
        let pairs = aliased().owner_alias_pairs();
        assert_eq!(
            pairs.get("hypermemetic").map(String::as_str),
            Some("hypermemetic-ai")
        );
        // the canonical owner is not itself an alias key.
        assert!(!pairs.contains_key("hypermemetic-ai"));
    }

    #[test]
    fn owner_aliases_round_trip_through_yaml() {
        // deny_unknown_fields must accept the new key, and it must resolve.
        let yaml = "owner_aliases:\n  hypermemetic-ai:\n    - hypermemetic\n";
        let g: GlobalConfig = serde_yaml::from_str(yaml).expect("owner_aliases parses");
        assert_eq!(g.canonical_owner("hypermemetic"), "hypermemetic-ai");
    }

    #[test]
    fn aliased_repo_drift_and_orgdiff_agree() {
        // all-sites-routed: the two drift/sync comparison shapes both agree
        // on an aliased repo when routed through canonical_owner.
        let g = aliased();
        // config-drift (workspaces.rs reconcile): declared user owner vs
        // workspace org — same identity, so NOT reported as drift.
        assert!(
            g.same_owner("hypermemetic", "hypermemetic-ai"),
            "config-drift must treat the alias as agreement, not drift"
        );
        // org-diff classify (workspaces.rs org_diff): a repo registered under
        // `hypermemetic-ai`, checked from `hypermemetic-ai` against some other
        // target org, whose forge answer is the alias `hypermemetic`. Routed
        // through canonical_owner it buckets as Stayed; on raw strings the
        // alias would mismatch both `from` and `to` and wrongly bucket as
        // Elsewhere.
        use crate::v5::ops::repo::OrgDiffVerdict;
        let forge_owner = g.canonical_owner("hypermemetic"); // forge says user
        let from_org = g.canonical_owner("hypermemetic-ai");
        let to_org = g.canonical_owner("other-org");
        assert_eq!(
            OrgDiffVerdict::classify(&forge_owner, &from_org, &to_org),
            OrgDiffVerdict::Stayed,
        );
        // Sanity: on raw strings (no canonicalization) it would be Elsewhere.
        assert_eq!(
            OrgDiffVerdict::classify("hypermemetic", "hypermemetic-ai", "other-org"),
            OrgDiffVerdict::Elsewhere,
        );
    }

    #[test]
    fn minimal_org_fixture_loads() {
        let cfg = load_all(&fixture_dir("minimal_org")).expect("minimal_org loads");
        assert_eq!(cfg.orgs.len(), 1);
        let demo = cfg.orgs.get(&OrgName("demo".into())).expect("demo org");
        assert_eq!(demo.name.as_str(), "demo");
        assert!(matches!(demo.forge.provider, ProviderKind::Github));
        assert!(demo.forge.credentials.is_empty());
        assert!(demo.repos.is_empty());
    }

    #[test]
    fn round_trip_minimal_org() {
        let first = load_all(&fixture_dir("minimal_org")).expect("load first");
        // Serialise each org, reparse, compare.
        for (_name, cfg) in &first.orgs {
            let yaml = serde_yaml::to_string(cfg).expect("serialise");
            let back: OrgConfig = serde_yaml::from_str(&yaml).expect("reparse");
            assert_eq!(&back, cfg);
        }
    }

    #[test]
    fn unknown_top_level_field_errors() {
        let bad = "name: demo\nforge: {provider: github, credentials: []}\nrepos: []\nextra: oops\n";
        let err = serde_yaml::from_str::<OrgConfig>(bad).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("extra"), "error must name the unknown field: {msg}");
    }

    #[test]
    fn unknown_provider_rejected() {
        let bad = "name: demo\nforge: {provider: bitbucket, credentials: []}\nrepos: []\n";
        let err = serde_yaml::from_str::<OrgConfig>(bad).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bitbucket") || msg.contains("unknown variant"),
            "error must name the unknown variant: {msg}"
        );
    }

    #[test]
    fn invalid_yaml_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "this: is: not: yaml:\n").unwrap();
        let err = load_global(&tmp.path().join("config.yaml")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("config.yaml"), "msg names config.yaml: {msg}");
    }

    #[test]
    fn basename_mismatch_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let orgs = tmp.path().join("orgs");
        std::fs::create_dir_all(&orgs).unwrap();
        std::fs::write(
            orgs.join("foo.yaml"),
            "name: bar\nforge: {provider: github, credentials: []}\nrepos: []\n",
        )
        .unwrap();
        let err = load_orgs(&orgs).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("foo"), "msg names basename: {msg}");
        assert!(msg.contains("bar"), "msg names in-file name: {msg}");
    }

    #[test]
    fn round_trip_global() {
        let cfg = GlobalConfig {
            default_workspace: Some(WorkspaceName("main".into())),
            provider_map: [(DomainName("github.com".into()), ProviderKind::Github)]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: GlobalConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, cfg);
    }
}
