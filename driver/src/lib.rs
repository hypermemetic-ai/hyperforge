//! hyperforge-driver — the contract every deploy driver implements and every
//! caller uses (HYPERFORGE-9).
//!
//! One bodiless `impl DriverActivation`, four outputs (MACRO-13):
//!
//! | output | what | build |
//! |---|---|---|
//! | the schema | every method and type, from the doc comments; any language through synapse-cc | any hub that mounts it |
//! | `trait Driver` | what a built-in driver implements; `Arc<dyn Driver>` in hyperforge | always |
//! | `DriverActivation<D>`, `Driver::server()` | serves any `D: Driver` on a hub | `server` |
//! | `DriverClient<T>: Driver` | a plugin, reached over a transport; menger's ENV-3 | always |
//!
//! ```
//! use hyperforge_driver::{not_supported, Action, BoxStream, Driver, DriverEvent, Inputs, RunId, Target, Decision};
//!
//! struct Nothing;
//! impl Driver for Nothing {
//!     fn describe(&self) -> BoxStream<'static, DriverEvent> { not_supported("describe") }
//!     fn trigger(&self, _: Target, _: Action, _: Inputs, _: String) -> BoxStream<'static, DriverEvent> { not_supported("trigger") }
//!     fn watch(&self, _: RunId) -> BoxStream<'static, DriverEvent> { not_supported("watch") }
//!     fn log(&self, _: RunId, _: Option<u64>) -> BoxStream<'static, DriverEvent> { not_supported("log") }
//!     fn cancel(&self, _: RunId) -> BoxStream<'static, DriverEvent> { not_supported("cancel") }
//!     fn approve(&self, _: RunId, _: Option<String>, _: Decision, _: String, _: Option<String>) -> BoxStream<'static, DriverEvent> { not_supported("approve") }
//! }
//! let _served = Nothing.server();
//! ```
//!
//! The shape is declared nowhere else: a driver whose method disagrees with
//! the activation's does not build.
//!
//! ```compile_fail,E0053
//! use hyperforge_driver::{not_supported, Action, BoxStream, Decision, Driver, DriverEvent, Inputs, RunId, Target};
//!
//! // `watch` as it was drafted (`RunId(u64)`): every method present, one retyped
//! struct Old;
//! impl Driver for Old {
//!     fn describe(&self) -> BoxStream<'static, DriverEvent> { not_supported("describe") }
//!     fn trigger(&self, _: Target, _: Action, _: Inputs, _: String) -> BoxStream<'static, DriverEvent> { not_supported("trigger") }
//!     fn watch(&self, _: u64) -> BoxStream<'static, DriverEvent> { not_supported("watch") }
//!     fn log(&self, _: RunId, _: Option<u64>) -> BoxStream<'static, DriverEvent> { not_supported("log") }
//!     fn cancel(&self, _: RunId) -> BoxStream<'static, DriverEvent> { not_supported("cancel") }
//!     fn approve(&self, _: RunId, _: Option<String>, _: Decision, _: String, _: Option<String>) -> BoxStream<'static, DriverEvent> { not_supported("approve") }
//! }
//! ```

mod record;
mod types;

#[cfg(feature = "server")]
use futures::Stream;

pub use plexus_client::BoxStream;
pub use record::{Record, RecordSource, RecordState};
pub use types::{
    codes, Action, ActionInput, ActionInputs, Capabilities, Decision, Described, DriverEvent,
    Gate, InputKind, Inputs, LogSupport, OrgName, RepoName, RepoRef, RunId, RunState, Target,
    TargetName,
};

/// A deploy driver: what carries an action out on a target — a script, the
/// Linode, GitHub Actions, a provider, a plugin on another computer.
#[plexus_macros::activation]
impl DriverActivation {
    /// What this driver can do: its actions, its targets, the inputs each action takes, and whether it has logs, cancel, approval and records of its own.
    pub async fn describe(&self) -> impl Stream<Item = DriverEvent> + Send + 'static;

    /// Start an action on a target: `run` first, then its states and log until a terminal state, then its `record` where the system sends none of its own.
    pub async fn trigger(
        &self,
        /** what to act on */ target: Target,
        /** what to do */ action: Action,
        /** name → value, as `describe` names them */ inputs: Inputs,
        /** why; kept on the run */ reason: String,
    ) -> impl Stream<Item = DriverEvent> + Send + 'static;

    /// Follow a run already started: its current state, then each change and log line until it ends.
    pub async fn watch(
        &self,
        /** the driver's run id */ run: RunId,
    ) -> impl Stream<Item = DriverEvent> + Send + 'static;

    /// The log lines kept after `after` (from the first, without it), then live ones until the run ends — or `error not_supported`.
    pub async fn log(
        &self,
        /** the driver's run id */ run: RunId,
        /** the last line already had (a `log` event's seq) */ after: Option<u64>,
    ) -> impl Stream<Item = DriverEvent> + Send + 'static;

    /// Ask a run to stop: `cancel_requested`, with `canceled` to follow on its stream — or `error not_supported`.
    pub async fn cancel(
        &self,
        /** the driver's run id */ run: RunId,
    ) -> impl Stream<Item = DriverEvent> + Send + 'static;

    /// Answer a waiting run's gate: approved, it goes on (`state running`); rejected, it ends — or `error not_supported`.
    pub async fn approve(
        &self,
        /** the driver's run id */ run: RunId,
        /** which gate, when it waits on more than one */ gate: Option<String>,
        /** approve or reject */ decision: Decision,
        /** who answered */ by: String,
        /** why */ reason: Option<String>,
    ) -> impl Stream<Item = DriverEvent> + Send + 'static;
}

/// One `error { code: "not_supported" }`, then the end: what a driver answers
/// for a method its `Capabilities` does not claim.
pub fn not_supported(method: &str) -> BoxStream<'static, DriverEvent> {
    let event = DriverEvent::error(codes::NOT_SUPPORTED, format!("this driver does not support {method}"));
    Box::pin(futures::stream::iter([event]))
}
