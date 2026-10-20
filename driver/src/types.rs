//! The contract's vocabulary: a target, an action, a run and its states.
//! Shaped by S1 (HYPERFORGE-8 → HYPERFORGE-25): menger's `deploy/pipeline`,
//! GitHub Actions and hyperforge's `ci`, one run each.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::record::Record;

/// A hyperforge org, as hyperforge v5 spells it on the wire (`OrgName`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct OrgName(pub String);

/// A repo within an org, as hyperforge v5 spells it on the wire (`RepoName`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RepoName(pub String);

/// `{ org, name }` — hyperforge v5's `RepoRefWire`, the same shape, declared
/// here so the contract does not depend on hyperforge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct RepoRef {
    pub org: OrgName,
    pub name: RepoName,
}

/// A thing a driver can act on: `menger/prod`, `plexus-core/crates.io`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct Target {
    pub repo: RepoRef,
    pub name: TargetName,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.repo.name.0, self.name.0)
    }
}

/// A target's name within its repo: `OrgName`'s constraint — non-empty,
/// ASCII, at most 64 bytes, no leading `.`, no `/`, `\` or NUL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct TargetName(String);

impl TargetName {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty() {
            return Err("a target name must not be empty".into());
        }
        if name.len() > 64 {
            return Err(format!("target name {name:?} exceeds 64 bytes"));
        }
        if !name.is_ascii() {
            return Err(format!("target name {name:?} is not ASCII"));
        }
        if name.starts_with('.') {
            return Err(format!("target name {name:?} must not start with '.'"));
        }
        if name.contains(['/', '\\', '\0']) {
            return Err(format!("target name {name:?} contains a path separator"));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TargetName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Deploy,
    Promote,
    Rollback,
    Redeploy,
    Sync,
    Publish,
}

/// What `describe` says one action takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActionInput {
    pub name: String,
    pub kind: InputKind,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// An input's kind. Every value travels as a string ([`Inputs`]); the kind
/// says how a caller should ask for it (GitHub's workflow inputs have all of
/// these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    String,
    Revision,
    Release,
    Choice,
    Boolean,
    Number,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActionInputs {
    pub action: Action,
    pub inputs: Vec<ActionInput>,
}

/// What a trigger gives: name → value, each named by `describe`.
pub type Inputs = BTreeMap<String, String>;

/// The driver's own id for a run, opaque: `deploy@2026-10-20T01:08:59Z#1424214`,
/// `39351836815`, `hf8-spike/1`. A driver whose system gives none mints one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RunId(pub String);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RunId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// A run's state. `waiting` is held for an approval (its gates are named on
/// the `state` event); `rolled_back` is a run that returned the target to
/// what ran before (menger's promote, DEPLOY-10). A system's other endings —
/// GitHub's `timed_out`, `startup_failure` — are `failed` with the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Waiting,
    Running,
    Succeeded,
    Failed,
    Canceled,
    RolledBack,
}

impl RunState {
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Canceled | Self::RolledBack)
    }
}

/// What a waiting run waits on: a GitHub environment's reviewers, a manual
/// stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Gate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An approval's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Reject,
}

/// Whether, and when, a driver has a run's log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogSupport {
    /// None: the system keeps its own, and the run's `url` points at it.
    #[default]
    None,
    /// Once a part of the run ends (GitHub serves a job's log only then).
    After,
    /// Line by line as it runs.
    Live,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    pub log: LogSupport,
    pub cancel: bool,
    pub approve: bool,
    /// The system sends its own DEPLOY-10 records (menger's `deploy/pipeline`
    /// does), so hyperforge must not send a second.
    pub records: bool,
}

/// What `describe` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Described {
    pub actions: Vec<Action>,
    pub targets: Vec<Target>,
    pub inputs: Vec<ActionInputs>,
    pub capabilities: Capabilities,
}

/// The in-band error codes every driver uses; a driver may add its own.
pub mod codes {
    /// The driver does not do this (its `Capabilities` said so).
    pub const NOT_SUPPORTED: &str = "not_supported";
    /// No run by that id.
    pub const UNKNOWN_RUN: &str = "unknown_run";
    /// The target, action or inputs are not ones `describe` named.
    pub const REFUSED: &str = "refused";
    /// The call did not reach the driver (a plugin's connection).
    pub const CLIENT: &str = "client";
}

/// Every driver method yields these. `at` is unix seconds — the system's
/// time where it gives one, else when the driver saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverEvent {
    Described {
        described: Described,
    },
    /// The run `trigger` started; first on its stream.
    Run {
        id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    State {
        run: RunId,
        state: RunState,
        at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// What a `waiting` run waits on.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gates: Vec<Gate>,
        /// A retried run's attempt (GitHub's re-run, a stage's second try).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
    },
    /// A log line. `trigger` and `watch` carry them live; `log` replays the
    /// kept ones after `seq`, then continues live.
    Log {
        run: RunId,
        /// The line's place in the run's log, from 1; what `log --after` counts.
        seq: u64,
        line: String,
        at: u64,
        /// `stdout`, `stderr`, or the system's own name for where it came from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        /// The job, stage or step it came from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
    },
    /// `cancel` was taken; `canceled` arrives on the run's stream when the
    /// system has stopped it.
    CancelRequested {
        run: RunId,
        at: u64,
    },
    /// The record the run ended in, where the system does not send its own.
    Record {
        run: RunId,
        record: Box<Record>,
    },
    Error {
        code: String,
        message: String,
    },
}

impl DriverEvent {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error { code: code.to_string(), message: message.into() }
    }
}

impl plexus_client::FromClientError for DriverEvent {
    fn from_client_error(err: plexus_client::ClientError) -> Self {
        Self::error(codes::CLIENT, err.to_string())
    }
}
