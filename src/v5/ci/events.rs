//! Wire shapes of the `ci` hub and its pipeline children (HYPERFORGE-22).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::spec::Trigger;

/// A stage's state on a run — its latest attempt's, or `pending` before any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl StageState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

/// One attempt at a stage on a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attempt {
    pub attempt: u32,
    pub state: StageState,
    pub triggered_by: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why it failed or was canceled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What the stage wrote to `$HF_OUTPUTS` (kept on success).
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

/// A stage on a run: its state and every attempt, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StageRecord {
    pub name: String,
    pub state: StageState,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
}

impl StageRecord {
    #[must_use]
    pub fn latest(&self) -> Option<&Attempt> {
        self.attempts.last()
    }
}

/// A run of a pipeline, as kept under the state dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunRecord {
    pub pipeline: String,
    pub run: u64,
    pub repo: String,
    pub started_by: String,
    pub started_at: String,
    pub stages: Vec<StageRecord>,
}

impl RunRecord {
    #[must_use]
    pub fn stage(&self, name: &str) -> Option<&StageRecord> {
        self.stages.iter().find(|s| s.name == name)
    }
}

/// A stage as declared, for `describe` and `pipelines`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StageView {
    pub name: String,
    pub run: String,
    pub needs: Vec<String>,
    pub trigger: Trigger,
    pub outputs: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u64>,
}

/// Events of `ci` and of each pipeline child.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CiEvent {
    /// A pipeline as declared: its repo, its env, its stages.
    Pipeline {
        name: String,
        repo: String,
        env: BTreeMap<String, String>,
        stages: Vec<StageView>,
    },
    /// `ci add`: the repo is registered with these pipelines.
    RepoAdded { path: String, pipelines: Vec<String> },
    /// `ci remove`: the repo is no longer registered.
    RepoRemoved { path: String, pipelines: Vec<String> },
    /// `start`: a new run, answered before any stage runs.
    RunStarted {
        pipeline: String,
        run: u64,
        started_by: String,
        started_at: String,
    },
    /// `trigger`: the stage's new attempt is under way.
    Triggered {
        pipeline: String,
        run: u64,
        stage: String,
        attempt: u32,
    },
    /// A run and every stage on it (`get`, `runs`, and first in `watch`).
    Run { run: RunRecord },
    StageStarted {
        pipeline: String,
        run: u64,
        stage: String,
        attempt: u32,
    },
    /// One line of a stage's stdout or stderr.
    Log {
        pipeline: String,
        run: u64,
        stage: String,
        attempt: u32,
        stream: String,
        line: String,
    },
    StageFinished {
        pipeline: String,
        run: u64,
        stage: String,
        attempt: u32,
        state: StageState,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        outputs: BTreeMap<String, String>,
    },
    /// `cancel`: the stage's process group is being stopped.
    CancelRequested {
        pipeline: String,
        run: u64,
        stage: String,
        attempt: u32,
    },
    /// Nothing is running on the run any more.
    Idle { pipeline: String, run: u64 },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

impl CiEvent {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: Some(code.to_string()),
            message: message.into(),
        }
    }

    /// The (pipeline, run) an event is about, if it is about one.
    #[must_use]
    pub const fn run_key(&self) -> Option<(&str, u64)> {
        match self {
            Self::RunStarted { pipeline, run, .. }
            | Self::Triggered { pipeline, run, .. }
            | Self::StageStarted { pipeline, run, .. }
            | Self::Log { pipeline, run, .. }
            | Self::StageFinished { pipeline, run, .. }
            | Self::CancelRequested { pipeline, run, .. }
            | Self::Idle { pipeline, run } => Some((pipeline.as_str(), *run)),
            Self::Run { run } => Some((run.pipeline.as_str(), run.run)),
            _ => None,
        }
    }
}
