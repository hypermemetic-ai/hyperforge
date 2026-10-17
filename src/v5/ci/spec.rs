//! `.hyperforge/pipelines.toml` — a repo's declared pipelines (HYPERFORGE-22).
//!
//! The file is read at every call, so an edit is live without a restart.
//! A file that does not parse is refused with its line (the `toml`
//! crate's error names it); a structurally wrong one (an unknown need,
//! a duplicate stage, a name that cannot be a path segment) is refused
//! naming the pipeline and the stage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a repo declares its pipelines, relative to the repo root.
pub const PIPELINES_FILE: &str = ".hyperforge/pipelines.toml";

/// When a stage runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Runs as soon as its needs have succeeded (at `start`, or after the
    /// stage it needs succeeds).
    Auto,
    /// Runs only when `trigger`ed.
    #[default]
    Manual,
}

/// One stage of a pipeline, as declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub name: String,
    /// Shell command, run under `sh -c` in the repo's directory.
    pub run: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub trigger: Trigger,
    /// Keys the stage must write to `$HF_OUTPUTS` to succeed.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Stage env; overrides the pipeline's.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u64>,
}

/// One pipeline, as declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineSpec {
    pub name: String,
    /// Pipeline-wide env; a stage's env overrides it.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, rename = "stage")]
    pub stages: Vec<StageSpec>,
}

impl PipelineSpec {
    #[must_use]
    pub fn stage(&self, name: &str) -> Option<&StageSpec> {
        self.stages.iter().find(|s| s.name == name)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelinesFile {
    #[serde(default)]
    pipeline: Vec<PipelineSpec>,
}

/// A pipeline together with the repo that declares it.
#[derive(Debug, Clone)]
pub struct Declared {
    pub repo: PathBuf,
    pub spec: PipelineSpec,
}

/// A name usable as a path segment in `ci <name> …`: no dots, no spaces.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether two pipeline names are one: `-` and `_` are the same letter, as
/// synapse turns `ci menger-app` into `ci.menger_app` on the wire.
#[must_use]
pub fn same_name(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.chars()
            .zip(b.chars())
            .all(|(x, y)| x == y || (matches!(x, '-' | '_') && matches!(y, '-' | '_')))
}

/// Parse the text of a `pipelines.toml`. `origin` names the file in errors.
pub fn parse(text: &str, origin: &str) -> Result<Vec<PipelineSpec>, String> {
    let file: PipelinesFile =
        toml::from_str(text).map_err(|e| format!("{origin}: {}", e.to_string().trim_end()))?;
    for (i, p) in file.pipeline.iter().enumerate() {
        if let Some(q) = file.pipeline[..i].iter().find(|q| same_name(&q.name, &p.name)) {
            return Err(format!(
                "{origin}: pipeline `{}` is declared twice (as `{}` too)",
                p.name, q.name
            ));
        }
    }
    for p in &file.pipeline {
        if !valid_name(&p.name) {
            return Err(format!(
                "{origin}: pipeline name `{}` must be letters, digits, `-` or `_`",
                p.name
            ));
        }
        let mut stages = BTreeSet::new();
        for s in &p.stages {
            if !valid_name(&s.name) {
                return Err(format!(
                    "{origin}: pipeline `{}`: stage name `{}` must be letters, digits, `-` or `_`",
                    p.name, s.name
                ));
            }
            if !stages.insert(s.name.clone()) {
                return Err(format!(
                    "{origin}: pipeline `{}`: stage `{}` is declared twice",
                    p.name, s.name
                ));
            }
        }
        // Needs name an earlier stage — so the order of the file is an order
        // of work and there is no cycle.
        let mut earlier = BTreeSet::new();
        for s in &p.stages {
            for n in &s.needs {
                if !earlier.contains(n) {
                    let why = if stages.contains(n) { "a later stage" } else { "no stage" };
                    return Err(format!(
                        "{origin}: pipeline `{}`: stage `{}` needs `{n}`, which is {why}",
                        p.name, s.name
                    ));
                }
            }
            earlier.insert(s.name.clone());
        }
    }
    Ok(file.pipeline)
}

/// Read and parse `<repo>/.hyperforge/pipelines.toml`.
pub fn load(repo: &Path) -> Result<Vec<PipelineSpec>, String> {
    let path = repo.join(PIPELINES_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text, &path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_tickets_example() {
        let text = r#"
[[pipeline]]
name = "menger-app"
env = { IMAGE_STORE = "local" }

[[pipeline.stage]]
name = "build"
run = "deploy/pipeline image"
trigger = "auto"
outputs = ["image", "release"]

[[pipeline.stage]]
name = "deploy-local"
needs = ["build"]
run = "deploy/pipeline local --image \"$IMAGE\""
timeout_minutes = 30
"#;
        let p = parse(text, "x").unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].stages[0].trigger, Trigger::Auto);
        assert_eq!(p[0].stages[1].trigger, Trigger::Manual);
        assert_eq!(p[0].stages[1].timeout_minutes, Some(30));
        assert_eq!(p[0].env["IMAGE_STORE"], "local");
    }

    #[test]
    fn a_syntax_error_names_its_line() {
        let err = parse("[[pipeline]]\nname = \"a\"\nrun = = 1\n", "f.toml").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn an_unknown_need_is_refused() {
        let text = "[[pipeline]]\nname = \"a\"\n[[pipeline.stage]]\nname = \"b\"\nrun = \"true\"\nneeds = [\"zz\"]\n";
        let err = parse(text, "f").unwrap_err();
        assert!(err.contains("needs `zz`"), "{err}");
    }
}
