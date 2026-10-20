//! A scripted driver: runs kept in memory, every state and line said on a
//! broadcast so `watch` and `log` follow live. `deploy` runs straight
//! through to `succeeded` and its record; `promote` waits on the gate
//! `prod` until `approve`; `cancel` stops anything not ended.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hyperforge_driver::{
    codes, Action, ActionInput, ActionInputs, BoxStream, Capabilities, Decision, Described, Driver,
    DriverEvent, Gate, InputKind, Inputs, LogSupport, OrgName, Record, RecordSource, RecordState,
    RepoName, RepoRef, RunId, RunState, Target, TargetName,
};
use tokio::sync::broadcast;

pub fn target(repo: &str, name: &str) -> Target {
    Target {
        repo: RepoRef { org: OrgName("hypermemetic".into()), name: RepoName(repo.into()) },
        name: TargetName::new(name).unwrap(),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

struct Run {
    target: Target,
    revision: String,
    events: Vec<DriverEvent>,
    state: RunState,
    lines: u64,
}

#[derive(Clone)]
pub struct Scripted {
    next: Arc<AtomicU64>,
    runs: Arc<Mutex<HashMap<RunId, Run>>>,
    tx: broadcast::Sender<DriverEvent>,
}

impl Default for Scripted {
    fn default() -> Self {
        Self::new()
    }
}

impl Scripted {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { next: Arc::new(AtomicU64::new(1)), runs: Arc::default(), tx }
    }

    fn described() -> Described {
        Described {
            actions: vec![Action::Deploy, Action::Promote],
            targets: vec![target("menger", "test"), target("menger", "prod")],
            inputs: vec![ActionInputs {
                action: Action::Deploy,
                inputs: vec![ActionInput {
                    name: "revision".into(),
                    kind: InputKind::Revision,
                    required: false,
                    choices: vec![],
                }],
            }],
            capabilities: Capabilities { log: LogSupport::Live, cancel: true, approve: true, records: false },
        }
    }

    /// Keep an event on its run and say it to every follower, under the lock.
    fn emit(runs: &mut HashMap<RunId, Run>, tx: &broadcast::Sender<DriverEvent>, id: &RunId, event: DriverEvent) {
        let run = runs.get_mut(id).expect("a kept run");
        if let DriverEvent::State { state, .. } = &event {
            run.state = *state;
        }
        run.events.push(event.clone());
        let _ = tx.send(event);
    }

    fn state(state: RunState, run: &RunId, reason: Option<&str>, gates: Vec<Gate>) -> DriverEvent {
        DriverEvent::State { run: run.clone(), state, at: now(), reason: reason.map(Into::into), gates, attempt: None }
    }

    fn line(runs: &mut HashMap<RunId, Run>, tx: &broadcast::Sender<DriverEvent>, id: &RunId, line: String) {
        let seq = {
            let run = runs.get_mut(id).expect("a kept run");
            run.lines += 1;
            run.lines
        };
        let event = DriverEvent::Log { run: id.clone(), seq, line, at: now(), stream: Some("stdout".into()), step: None, attempt: None };
        Self::emit(runs, tx, id, event);
    }

    fn finish(runs: &mut HashMap<RunId, Run>, tx: &broadcast::Sender<DriverEvent>, id: &RunId) {
        Self::line(runs, tx, id, "smoke passed".into());
        Self::emit(runs, tx, id, Self::state(RunState::Succeeded, id, None, vec![]));
        let run = &runs[id];
        let record = Record {
            project: run.target.repo.name.0.clone(),
            environment: run.target.name.as_str().to_string(),
            state: RecordState::Succeeded,
            revision: run.revision.clone(),
            previous: None,
            commits: vec![],
            tickets: vec![],
            release: None,
            at: None,
            from: None,
            request: None,
            source: Some(RecordSource { system: "scripted".into(), run: Some(id.0.clone()), url: None }),
        };
        Self::emit(runs, tx, id, DriverEvent::Record { run: id.clone(), record: Box::new(record) });
    }

    /// The run's kept events matching `keep`, then live ones until it ends.
    fn follow(&self, id: RunId, keep: impl Fn(&DriverEvent) -> bool + Send + 'static) -> BoxStream<'static, DriverEvent> {
        let (kept, ended, mut rx) = {
            let runs = self.runs.lock().unwrap();
            let Some(run) = runs.get(&id) else {
                let e = DriverEvent::error(codes::UNKNOWN_RUN, format!("no run {id}"));
                return Box::pin(futures::stream::iter([e]));
            };
            let kept: Vec<_> = run.events.iter().filter(|e| keep(e)).cloned().collect();
            (kept, run.state.terminal(), self.tx.subscribe())
        };
        Box::pin(async_stream::stream! {
            for e in kept { yield e; }
            if ended { return; }
            while let Ok(e) = rx.recv().await {
                let (mine, end) = match &e {
                    // a succeeded run ends at its record, which follows the state
                    DriverEvent::State { run, state, .. } => (run == &id, run == &id && state.terminal() && *state != RunState::Succeeded),
                    DriverEvent::Record { run, .. } => (run == &id, run == &id),
                    DriverEvent::Log { run, .. } | DriverEvent::CancelRequested { run, .. } => (run == &id, false),
                    _ => (false, false),
                };
                if mine && keep(&e) { yield e; }
                if end { break; }
            }
        })
    }
}

impl Driver for Scripted {
    fn describe(&self) -> BoxStream<'static, DriverEvent> {
        Box::pin(futures::stream::iter([DriverEvent::Described { described: Self::described() }]))
    }

    fn trigger(&self, target: Target, action: Action, inputs: Inputs, reason: String) -> BoxStream<'static, DriverEvent> {
        let d = Self::described();
        if !d.targets.contains(&target) || !d.actions.contains(&action) {
            let e = DriverEvent::error(codes::REFUSED, format!("{action:?} on {target} is not a target and action this driver has"));
            return Box::pin(futures::stream::iter([e]));
        }
        let id = RunId(format!("scripted-{}", self.next.fetch_add(1, Ordering::Relaxed)));
        let revision = inputs.get("revision").cloned().unwrap_or_else(|| "0000000".into());
        {
            let mut runs = self.runs.lock().unwrap();
            runs.insert(id.clone(), Run { target: target.clone(), revision, events: vec![], state: RunState::Queued, lines: 0 });
            let runs = &mut *runs;
            Self::emit(runs, &self.tx, &id, DriverEvent::Run { id: id.clone(), url: None });
            Self::emit(runs, &self.tx, &id, Self::state(RunState::Queued, &id, None, vec![]));
            Self::emit(runs, &self.tx, &id, Self::state(RunState::Running, &id, None, vec![]));
            Self::line(runs, &self.tx, &id, format!("{action:?} {target}: {reason}"));
            if action == Action::Promote {
                let gate = Gate { name: "prod".into(), description: Some("a reviewer approves prod".into()) };
                Self::emit(runs, &self.tx, &id, Self::state(RunState::Waiting, &id, Some("needs approval"), vec![gate]));
            } else {
                Self::finish(runs, &self.tx, &id);
            }
        }
        self.follow(id, |_| true)
    }

    fn watch(&self, run: RunId) -> BoxStream<'static, DriverEvent> {
        self.follow(run, |e| !matches!(e, DriverEvent::Run { .. }))
    }

    fn log(&self, run: RunId, after: Option<u64>) -> BoxStream<'static, DriverEvent> {
        let after = after.unwrap_or(0);
        self.follow(run, move |e| matches!(e, DriverEvent::Log { seq, .. } if *seq > after))
    }

    fn cancel(&self, run: RunId) -> BoxStream<'static, DriverEvent> {
        let mut runs = self.runs.lock().unwrap();
        let answer = match runs.get(&run).map(|r| r.state) {
            None => DriverEvent::error(codes::UNKNOWN_RUN, format!("no run {run}")),
            Some(s) if s.terminal() => DriverEvent::error(codes::REFUSED, format!("run {run} has ended")),
            Some(_) => {
                let asked = DriverEvent::CancelRequested { run: run.clone(), at: now() };
                Self::emit(&mut runs, &self.tx, &run, asked.clone());
                Self::emit(&mut runs, &self.tx, &run, Self::state(RunState::Canceled, &run, Some("canceled"), vec![]));
                asked
            }
        };
        Box::pin(futures::stream::iter([answer]))
    }

    fn approve(&self, run: RunId, gate: Option<String>, decision: Decision, by: String, _reason: Option<String>) -> BoxStream<'static, DriverEvent> {
        let mut runs = self.runs.lock().unwrap();
        let answer = match runs.get(&run).map(|r| r.state) {
            None => DriverEvent::error(codes::UNKNOWN_RUN, format!("no run {run}")),
            Some(RunState::Waiting) if gate.as_deref().is_some_and(|g| g != "prod") => {
                DriverEvent::error(codes::REFUSED, format!("run {run} waits on prod, not {}", gate.unwrap_or_default()))
            }
            Some(RunState::Waiting) => match decision {
                Decision::Approve => {
                    let going = Self::state(RunState::Running, &run, Some(&format!("approved by {by}")), vec![]);
                    Self::emit(&mut runs, &self.tx, &run, going.clone());
                    Self::finish(&mut runs, &self.tx, &run);
                    going
                }
                Decision::Reject => {
                    let ended = Self::state(RunState::Failed, &run, Some(&format!("rejected by {by}")), vec![]);
                    Self::emit(&mut runs, &self.tx, &run, ended.clone());
                    ended
                }
            },
            Some(_) => DriverEvent::error(codes::REFUSED, format!("run {run} is not waiting")),
        };
        Box::pin(futures::stream::iter([answer]))
    }
}
