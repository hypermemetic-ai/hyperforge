//! DEPLOY-10's deployment record, field for field against menger's
//! `deploy/record.schema.json` (vendored as `tests/fixtures/record.schema.json`;
//! `tests/record_schema.rs` holds the two together). Re-declared rather than
//! taken from menger-tickets-api so the contract stays light and wasm-safe
//! (HYPERFORGE-25).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What one deployment did: what menger's `deployments.record` takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Whose environment: a shortcode, or a project name menger maps to one.
    pub project: String,
    /// The environment's name within its project: `test`, `prod`, `crates.io`.
    pub environment: String,
    pub state: RecordState,
    /// The commit now running (for `failed`: the one tried); `repo@hash` outside the project's repository.
    pub revision: String,
    /// The commit that ran before, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Every commit in `previous..revision`, oldest first, when the driver can list them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    /// Ticket ids the driver names itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<String>,
    /// The artifact's name, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// When, in UTC: RFC 3339 with a `Z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// DEPLOY-21: the environment this one promotes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// DEPLOY-25: the request this deployment answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RecordSource>,
}

/// DEPLOY-10's four states. A run's states are richer ([`crate::RunState`]);
/// these are what menger keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    Started,
    Succeeded,
    Failed,
    RolledBack,
}

/// Where the record came from and where to read the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordSource {
    /// `hyperforge`, `menger-pipeline`, `github-actions`, or any name a system gives itself.
    pub system: String,
    /// The run's identity in that system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Where to read the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
