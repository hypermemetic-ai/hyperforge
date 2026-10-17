//! `CiHub` and `PipelineHub` — the wire surface of pipelines (HYPERFORGE-22).
//!
//! `ci` is a child of the v5 root. Each registered pipeline is a dynamic
//! child of `ci`, routed through the generated `ChildRouter::get_child`:
//! `synapse -P 44104 lforge-v5 hyperforge ci <pipeline> <method>`. A
//! pipeline's declaration is read from its repo at every call.

use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use plexus_core::plexus::{Activation, ChildSummary};
use tokio::sync::{broadcast, mpsc};

use super::events::{CiEvent, StageView};
use super::registry;
use super::runner::{self, Begun};
use super::spec::Declared;
use super::store::CiStore;

fn who(by: Option<String>) -> String {
    by.filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn pipeline_event(d: &Declared) -> CiEvent {
    CiEvent::Pipeline {
        name: d.spec.name.clone(),
        repo: d.repo.display().to_string(),
        env: d.spec.env.clone(),
        stages: d
            .spec
            .stages
            .iter()
            .map(|s| StageView {
                name: s.name.clone(),
                run: s.run.clone(),
                needs: s.needs.clone(),
                trigger: s.trigger,
                outputs: s.outputs.clone(),
                env: s.env.clone(),
                timeout_minutes: s.timeout_minutes,
            })
            .collect(),
    }
}

/// The `ci` namespace: registered repos and their pipelines.
#[derive(Clone)]
pub struct CiHub {
    config_dir: PathBuf,
    store: Arc<CiStore>,
}

impl CiHub {
    #[must_use]
    pub const fn new(config_dir: PathBuf, store: Arc<CiStore>) -> Self {
        Self { config_dir, store }
    }

    /// A pipeline child's hash: its schema's, its name, its repo and its
    /// declaration. Synapse caches a child's schema by this hash and treats
    /// a hash seen twice as a cycle, so each pipeline needs its own.
    fn child_hash(&self, d: &Declared) -> String {
        use std::hash::{Hash, Hasher};
        let child = PipelineHub {
            name: d.spec.name.clone(),
            config_dir: self.config_dir.clone(),
            store: self.store.clone(),
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        Activation::plugin_schema(&child).hash.hash(&mut h);
        d.spec.name.hash(&mut h);
        d.repo.hash(&mut h);
        serde_json::to_string(&d.spec).unwrap_or_default().hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

#[plexus_macros::activation(
    namespace = "ci",
    description = "Pipelines declared in repos' .hyperforge/pipelines.toml; each pipeline is a child (HYPERFORGE-22)",
    crate_path = "plexus_core"
)]
impl CiHub {
    /// A registered pipeline, by name.
    #[plexus_macros::child(list = "pipeline_names")]
    fn pipeline(&self, name: &str) -> Option<PipelineHub> {
        registry::find(&self.config_dir, name).map(|d| PipelineHub {
            name: d.spec.name,
            config_dir: self.config_dir.clone(),
            store: self.store.clone(),
        })
    }

    /// Names of every registered pipeline (for `list_children`), spelled as
    /// in `plugin_children`.
    fn pipeline_names(&self) -> impl Stream<Item = String> + Send + 'static {
        let names: Vec<String> = registry::declared(&self.config_dir)
            .map(|(d, _)| d.into_iter().map(|d| d.spec.name.replace('-', "_")).collect())
            .unwrap_or_default();
        futures::stream::iter(names)
    }

    /// Every registered pipeline, as a child in the schema. The namespace is
    /// spelled with `_` for `-`: synapse turns a typed `menger-app` into
    /// `menger_app` before it looks the child up, and `get_child` takes either.
    pub fn plugin_children(&self) -> Vec<ChildSummary> {
        registry::declared(&self.config_dir)
            .map(|(d, _)| {
                d.into_iter()
                    .map(|d| ChildSummary {
                        description: format!(
                            "Pipeline `{}` from {} ({} stages)",
                            d.spec.name,
                            d.repo.display(),
                            d.spec.stages.len()
                        ),
                        namespace: d.spec.name.replace('-', "_"),
                        hash: self.child_hash(&d),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Register a repo's pipelines (its .hyperforge/pipelines.toml). The path
    /// is kept in the config dir; a name already registered from another
    /// repo is refused, and so is a file that does not parse.
    #[plexus_macros::method(params(path = "The repo's directory"))]
    pub async fn add(&self, path: String) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            match registry::add(&config_dir, &path) {
                Ok((repo, pipelines)) => yield CiEvent::RepoAdded {
                    path: repo.display().to_string(),
                    pipelines,
                },
                Err(e) => yield CiEvent::error("refused", e),
            }
        }
    }

    /// Unregister a repo. Its runs stay kept.
    #[plexus_macros::method(params(path = "The repo's directory, as registered"))]
    pub async fn remove(&self, path: String) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            match registry::remove(&config_dir, &path) {
                Ok((repo, pipelines)) => yield CiEvent::RepoRemoved {
                    path: repo.display().to_string(),
                    pipelines,
                },
                Err(e) => yield CiEvent::error("not_found", e),
            }
        }
    }

    /// Every registered pipeline: its repo and its stages. A registered repo
    /// whose file no longer reads is said as an error.
    #[plexus_macros::method]
    pub async fn pipelines(&self) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            match registry::declared(&config_dir) {
                Ok((declared, broken)) => {
                    for d in &declared {
                        yield pipeline_event(d);
                    }
                    for e in broken {
                        yield CiEvent::error("invalid_pipelines", e);
                    }
                }
                Err(e) => yield CiEvent::error("registry", e),
            }
        }
    }
}

/// One pipeline: its runs, its stages, its triggers.
#[derive(Clone)]
pub struct PipelineHub {
    name: String,
    config_dir: PathBuf,
    store: Arc<CiStore>,
}

impl PipelineHub {
    fn pipeline_name(&self) -> &str {
        &self.name
    }
}

#[plexus_macros::activation(
    namespace = "pipeline",
    namespace_fn = "pipeline_name",
    description = "A pipeline: start a run, trigger its stages, read its runs and logs",
    crate_path = "plexus_core"
)]
impl PipelineHub {
    /// The pipeline as declared: its stages with their needs, trigger,
    /// outputs and env.
    #[plexus_macros::method]
    pub async fn describe(&self) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let name = self.name.clone();
        stream! {
            match registry::find(&config_dir, &name) {
                Some(d) => yield pipeline_event(&d),
                None => yield CiEvent::error("not_found", format!("no pipeline `{name}`")),
            }
        }
    }

    /// Start a new run: answers its number at once, then runs every `auto`
    /// stage whose needs are met, in order, streaming their states and logs.
    #[plexus_macros::method(params(by = "Who starts it (default: the daemon's user)"))]
    pub async fn start(&self, by: Option<String>) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let name = self.name.clone();
        let store = self.store.clone();
        let by = who(by);
        stream! {
            let Some(decl) = registry::find(&config_dir, &name) else {
                yield CiEvent::error("not_found", format!("no pipeline `{name}`"));
                return;
            };
            let stages: Vec<String> = decl.spec.stages.iter().map(|s| s.name.clone()).collect();
            let rec = match store.new_run(&name, &decl.repo, &stages, &by) {
                Ok(r) => r,
                Err(e) => {
                    yield CiEvent::error("store", e);
                    return;
                }
            };
            yield CiEvent::RunStarted {
                pipeline: name.clone(),
                run: rec.run,
                started_by: rec.started_by.clone(),
                started_at: rec.started_at.clone(),
            };
            let (tx, mut rx) = mpsc::unbounded_channel();
            store.enter(&name, rec.run);
            tokio::spawn(runner::drive(store.clone(), decl, rec.run, None, by, Some(tx)));
            while let Some(ev) = rx.recv().await {
                yield ev;
            }
        }
    }

    /// Run a stage on a run, with the outputs of its needs from that run.
    /// Refused while a need has not succeeded there. Again is a new attempt.
    /// When it succeeds, the `auto` stages that need it run.
    #[plexus_macros::method(params(
        run = "Run number",
        stage = "Stage name",
        by = "Who triggers it (default: the daemon's user)"
    ))]
    pub async fn trigger(
        &self,
        run: u64,
        stage: String,
        by: Option<String>,
    ) -> impl Stream<Item = CiEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let name = self.name.clone();
        let store = self.store.clone();
        let by = who(by);
        stream! {
            let Some(decl) = registry::find(&config_dir, &name) else {
                yield CiEvent::error("not_found", format!("no pipeline `{name}`"));
                return;
            };
            let Some(spec) = decl.spec.stage(&stage).cloned() else {
                yield CiEvent::error("not_found", format!("pipeline `{name}` has no stage `{stage}`"));
                return;
            };
            let (attempt, cancel, needed) =
                match store.begin_attempt(&name, run, &stage, &spec.needs, &by) {
                    Ok(b) => b,
                    Err(e) => {
                        yield CiEvent::error("refused", e);
                        return;
                    }
                };
            yield CiEvent::Triggered { pipeline: name.clone(), run, stage: stage.clone(), attempt };
            let (tx, mut rx) = mpsc::unbounded_channel();
            store.enter(&name, run);
            let begun = Begun { stage, attempt, cancel, needed };
            tokio::spawn(runner::drive(store.clone(), decl, run, Some(begun), by, Some(tx)));
            while let Some(ev) = rx.recv().await {
                yield ev;
            }
        }
    }

    /// The pipeline's runs, newest first.
    #[plexus_macros::method(params(limit = "At most this many (default 20)"))]
    pub async fn runs(&self, limit: Option<u32>) -> impl Stream<Item = CiEvent> + Send + 'static {
        let store = self.store.clone();
        let name = self.name.clone();
        stream! {
            for run in store.list(&name, limit.unwrap_or(20) as usize) {
                yield CiEvent::Run { run };
            }
        }
    }

    /// One run: each stage's state, attempts, outputs and times.
    #[plexus_macros::method(params(run = "Run number"))]
    pub async fn get(&self, run: u64) -> impl Stream<Item = CiEvent> + Send + 'static {
        let store = self.store.clone();
        let name = self.name.clone();
        stream! {
            match store.load(&name, run) {
                Some(run) => yield CiEvent::Run { run },
                None => yield CiEvent::error("not_found", format!("pipeline `{name}` has no run {run}")),
            }
        }
    }

    /// A run's current state, then each change (`stage_started`, `log`,
    /// `stage_finished`) until nothing runs on it; with --follow true it stays
    /// open.
    #[plexus_macros::method(params(
        run = "Run number",
        follow = "Stay open after the run is idle (default false)"
    ))]
    pub async fn watch(
        &self,
        run: u64,
        follow: Option<bool>,
    ) -> impl Stream<Item = CiEvent> + Send + 'static {
        let store = self.store.clone();
        let name = self.name.clone();
        let follow = follow.unwrap_or(false);
        // Subscribed before the snapshot, so nothing between them is lost.
        let mut rx = store.subscribe();
        stream! {
            let Some(snapshot) = store.load(&name, run) else {
                yield CiEvent::error("not_found", format!("pipeline `{name}` has no run {run}"));
                return;
            };
            yield CiEvent::Run { run: snapshot };
            if !follow && !store.busy(&name, run) {
                return;
            }
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev.run_key() != Some((name.as_str(), run)) {
                            continue;
                        }
                        let idle = matches!(ev, CiEvent::Idle { .. });
                        yield ev;
                        if idle && !follow {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        yield CiEvent::error("lagged", format!("{n} events were dropped"));
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }

    /// A stage's log on a run, line by line (its latest attempt unless
    /// --attempt names one).
    #[plexus_macros::method(params(
        run = "Run number",
        stage = "Stage name",
        attempt = "Attempt number (default: the latest)"
    ))]
    pub async fn log(
        &self,
        run: u64,
        stage: String,
        attempt: Option<u32>,
    ) -> impl Stream<Item = CiEvent> + Send + 'static {
        let store = self.store.clone();
        let name = self.name.clone();
        stream! {
            let Some(rec) = store.load(&name, run) else {
                yield CiEvent::error("not_found", format!("pipeline `{name}` has no run {run}"));
                return;
            };
            let latest = rec.stage(&stage).and_then(|s| s.latest()).map(|a| a.attempt);
            let Some(attempt) = attempt.or(latest) else {
                yield CiEvent::error("not_found", format!("`{stage}` has not run on run {run}"));
                return;
            };
            let text = std::fs::read_to_string(store.log_path(&name, run, &stage, attempt))
                .unwrap_or_default();
            for line in text.lines() {
                yield CiEvent::Log {
                    pipeline: name.clone(),
                    run,
                    stage: stage.clone(),
                    attempt,
                    stream: "log".to_string(),
                    line: line.to_string(),
                };
            }
        }
    }

    /// Stop a running stage and its whole process group; it ends canceled.
    #[plexus_macros::method(params(run = "Run number", stage = "Stage name"))]
    pub async fn cancel(&self, run: u64, stage: String) -> impl Stream<Item = CiEvent> + Send + 'static {
        let store = self.store.clone();
        let name = self.name.clone();
        stream! {
            match store.cancel(&name, run, &stage) {
                Ok(attempt) => yield CiEvent::CancelRequested { pipeline: name, run, stage, attempt },
                Err(e) => yield CiEvent::error("refused", e),
            }
        }
    }
}
