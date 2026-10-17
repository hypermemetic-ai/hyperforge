//! Runs kept under hyperforge's state dir (HYPERFORGE-22).
//!
//! Layout, under `<state_dir>/ci/`:
//!
//! ```text
//! <pipeline>/next                     # the next run number (never reused)
//! <pipeline>/<run>/run.json           # the run: stages, attempts, outputs, times
//! <pipeline>/<run>/<stage>.<n>.log    # attempt n's stdout + stderr, line by line
//! <pipeline>/<run>/<stage>.<n>.outputs  # attempt n's $HF_OUTPUTS
//! ```
//!
//! Every read goes to disk, so a reload of the store (a restart) sees what
//! was kept. Opening the store marks every attempt still `running` as
//! `failed` ("hyperforge restarted"): no process from before survives it.
//! Read-modify-write of a run is serialised by one lock, never held across
//! an await.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc, Notify};

use super::events::{Attempt, CiEvent, RunRecord, StageRecord, StageState};

/// The reason an attempt interrupted by a restart carries.
pub const RESTARTED: &str = "hyperforge restarted";

type StageKey = (String, u64, String);
type RunKey = (String, u64);
/// A begun attempt: its number, its cancel signal, and each need's outputs.
pub type BegunAttempt = (u32, Arc<Notify>, Vec<(String, BTreeMap<String, String>)>);

#[derive(Default)]
struct Live {
    /// The cancel signal of each running attempt.
    cancels: HashMap<StageKey, (u32, Arc<Notify>)>,
    /// How many drives are working on each run.
    drives: HashMap<RunKey, usize>,
}

/// The run store and the live state beside it.
pub struct CiStore {
    root: PathBuf,
    files: Mutex<()>,
    live: Mutex<Live>,
    events: broadcast::Sender<CiEvent>,
}

impl std::fmt::Debug for CiStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiStore").field("root", &self.root).finish_non_exhaustive()
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl CiStore {
    /// Open the store rooted at `<state_dir>/ci`, failing every attempt a
    /// previous process left `running`.
    #[must_use]
    pub fn open(state_dir: &Path) -> Self {
        let (events, _) = broadcast::channel(1024);
        let store = Self {
            root: state_dir.join("ci"),
            files: Mutex::new(()),
            live: Mutex::new(Live::default()),
            events,
        };
        store.recover();
        store
    }

    fn recover(&self) {
        let Ok(pipelines) = std::fs::read_dir(&self.root) else { return };
        for p in pipelines.flatten() {
            let Ok(runs) = std::fs::read_dir(p.path()) else { continue };
            for r in runs.flatten() {
                let path = r.path().join("run.json");
                let Some(mut rec) = read_json(&path) else { continue };
                let mut touched = false;
                for s in &mut rec.stages {
                    if let Some(a) = s.attempts.last_mut() {
                        if a.state == StageState::Running {
                            a.state = StageState::Failed;
                            a.reason = Some(RESTARTED.to_string());
                            a.finished_at = Some(now());
                            s.state = StageState::Failed;
                            touched = true;
                        }
                    }
                }
                if touched {
                    let _ = write_json(&path, &rec);
                }
            }
        }
    }

    fn pipeline_dir(&self, pipeline: &str) -> PathBuf {
        self.root.join(pipeline)
    }

    fn run_dir(&self, pipeline: &str, run: u64) -> PathBuf {
        self.pipeline_dir(pipeline).join(run.to_string())
    }

    #[must_use]
    pub fn log_path(&self, pipeline: &str, run: u64, stage: &str, attempt: u32) -> PathBuf {
        self.run_dir(pipeline, run).join(format!("{stage}.{attempt}.log"))
    }

    #[must_use]
    pub fn outputs_path(&self, pipeline: &str, run: u64, stage: &str, attempt: u32) -> PathBuf {
        self.run_dir(pipeline, run).join(format!("{stage}.{attempt}.outputs"))
    }

    /// Read a run from disk.
    #[must_use]
    pub fn load(&self, pipeline: &str, run: u64) -> Option<RunRecord> {
        read_json(&self.run_dir(pipeline, run).join("run.json"))
    }

    /// The pipeline's runs, newest first.
    #[must_use]
    pub fn list(&self, pipeline: &str, limit: usize) -> Vec<RunRecord> {
        let mut nums: Vec<u64> = std::fs::read_dir(self.pipeline_dir(pipeline))
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        nums.sort_unstable_by(|a, b| b.cmp(a));
        nums.into_iter()
            .filter_map(|n| self.load(pipeline, n))
            .take(limit)
            .collect()
    }

    /// A new run with every stage pending, numbered from the pipeline's
    /// counter (never reused).
    pub fn new_run(
        &self,
        pipeline: &str,
        repo: &Path,
        stages: &[String],
        by: &str,
    ) -> Result<RunRecord, String> {
        let _g = lock(&self.files);
        let dir = self.pipeline_dir(pipeline);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let next_path = dir.join("next");
        let from_counter: u64 = std::fs::read_to_string(&next_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        // Never below a run already on disk, whatever the counter says.
        let on_disk = std::fs::read_dir(&dir).map_or(0, |d| {
            d.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u64>().ok()))
                .max()
                .unwrap_or(0)
        });
        let run = from_counter.max(on_disk + 1);
        std::fs::write(&next_path, (run + 1).to_string())
            .map_err(|e| format!("{}: {e}", next_path.display()))?;
        let rec = RunRecord {
            pipeline: pipeline.to_string(),
            run,
            repo: repo.display().to_string(),
            started_by: by.to_string(),
            started_at: now(),
            stages: stages
                .iter()
                .map(|name| StageRecord {
                    name: name.clone(),
                    state: StageState::Pending,
                    attempts: Vec::new(),
                })
                .collect(),
        };
        let rdir = self.run_dir(pipeline, run);
        std::fs::create_dir_all(&rdir).map_err(|e| format!("{}: {e}", rdir.display()))?;
        write_json(&rdir.join("run.json"), &rec)?;
        Ok(rec)
    }

    /// Begin a new attempt at `stage` on `run`: refused while it runs, and
    /// refused while a need has not succeeded on this run (naming the need
    /// and its state). Answers the attempt number, its cancel signal, and
    /// the latest outputs of each need.
    pub fn begin_attempt(
        &self,
        pipeline: &str,
        run: u64,
        stage: &str,
        needs: &[String],
        by: &str,
    ) -> Result<BegunAttempt, String> {
        let _g = lock(&self.files);
        let path = self.run_dir(pipeline, run).join("run.json");
        let mut rec: RunRecord =
            read_json(&path).ok_or_else(|| format!("pipeline `{pipeline}` has no run {run}"))?;
        let mut needed = Vec::new();
        for n in needs {
            let state = rec.stage(n).map_or(StageState::Pending, |s| s.state);
            if state != StageState::Succeeded {
                return Err(format!(
                    "`{stage}` needs `{n}`, which is {} on run {run}",
                    state.as_str()
                ));
            }
            let outputs = rec
                .stage(n)
                .and_then(StageRecord::latest)
                .map(|a| a.outputs.clone())
                .unwrap_or_default();
            needed.push((n.clone(), outputs));
        }
        if rec.stage(stage).is_none() {
            // Declared after the run began: it joins the run now.
            rec.stages.push(StageRecord {
                name: stage.to_string(),
                state: StageState::Pending,
                attempts: Vec::new(),
            });
        }
        let s = rec
            .stages
            .iter_mut()
            .find(|s| s.name == stage)
            .ok_or_else(|| format!("no stage `{stage}`"))?;
        if s.state == StageState::Running {
            return Err(format!("`{stage}` is already running on run {run}"));
        }
        let attempt = u32::try_from(s.attempts.len()).unwrap_or(u32::MAX - 1) + 1;
        s.attempts.push(Attempt {
            attempt,
            state: StageState::Running,
            triggered_by: by.to_string(),
            started_at: now(),
            finished_at: None,
            exit_code: None,
            reason: None,
            outputs: BTreeMap::new(),
        });
        s.state = StageState::Running;
        write_json(&path, &rec)?;
        let notify = Arc::new(Notify::new());
        lock(&self.live).cancels.insert(
            (pipeline.to_string(), run, stage.to_string()),
            (attempt, notify.clone()),
        );
        Ok((attempt, notify, needed))
    }

    /// Record how an attempt ended.
    pub fn finish_attempt(
        &self,
        pipeline: &str,
        run: u64,
        stage: &str,
        attempt: u32,
        state: StageState,
        exit_code: Option<i32>,
        reason: Option<String>,
        outputs: BTreeMap<String, String>,
    ) -> Result<(), String> {
        {
            let mut live = lock(&self.live);
            let key = (pipeline.to_string(), run, stage.to_string());
            if live.cancels.get(&key).is_some_and(|(a, _)| *a == attempt) {
                live.cancels.remove(&key);
            }
        }
        let _g = lock(&self.files);
        let path = self.run_dir(pipeline, run).join("run.json");
        let mut rec: RunRecord =
            read_json(&path).ok_or_else(|| format!("pipeline `{pipeline}` has no run {run}"))?;
        let s = rec
            .stages
            .iter_mut()
            .find(|s| s.name == stage)
            .ok_or_else(|| format!("no stage `{stage}`"))?;
        if let Some(a) = s.attempts.iter_mut().find(|a| a.attempt == attempt) {
            a.state = state;
            a.exit_code = exit_code;
            a.reason = reason;
            a.outputs = outputs;
            a.finished_at = Some(now());
        }
        if s.attempts.last().is_some_and(|a| a.attempt == attempt) {
            s.state = state;
        }
        write_json(&path, &rec)
    }

    /// Signal the running attempt of `stage` to stop. Answers its number.
    pub fn cancel(&self, pipeline: &str, run: u64, stage: &str) -> Result<u32, String> {
        let live = lock(&self.live);
        if let Some((attempt, n)) = live
            .cancels
            .get(&(pipeline.to_string(), run, stage.to_string()))
        {
            n.notify_one();
            return Ok(*attempt);
        }
        drop(live);
        let state = self
            .load(pipeline, run)
            .ok_or_else(|| format!("pipeline `{pipeline}` has no run {run}"))?
            .stage(stage)
            .map_or(StageState::Pending, |s| s.state);
        Err(format!(
            "`{stage}` is not running on run {run} (it is {})",
            state.as_str()
        ))
    }

    /// A drive starts working on a run.
    pub fn enter(&self, pipeline: &str, run: u64) {
        *lock(&self.live)
            .drives
            .entry((pipeline.to_string(), run))
            .or_default() += 1;
    }

    /// A drive is done; answers whether the run is now idle.
    pub fn leave(&self, pipeline: &str, run: u64) -> bool {
        let mut live = lock(&self.live);
        let key = (pipeline.to_string(), run);
        let n = live.drives.entry(key.clone()).or_default();
        *n = n.saturating_sub(1);
        if *n == 0 {
            live.drives.remove(&key);
            true
        } else {
            false
        }
    }

    /// Whether any drive works on the run.
    #[must_use]
    pub fn busy(&self, pipeline: &str, run: u64) -> bool {
        lock(&self.live)
            .drives
            .contains_key(&(pipeline.to_string(), run))
    }

    /// Send an event to every watcher, and to the caller's stream if any.
    pub fn emit(&self, ev: CiEvent, tx: Option<&mpsc::UnboundedSender<CiEvent>>) {
        if let Some(tx) = tx {
            let _ = tx.send(ev.clone());
        }
        let _ = self.events.send(ev);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<CiEvent> {
        self.events.subscribe()
    }
}

fn read_json(path: &Path) -> Option<RunRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json(path: &Path, rec: &RunRecord) -> Result<(), String> {
    let text = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}
