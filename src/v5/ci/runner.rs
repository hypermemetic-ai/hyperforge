//! Running stages: the local shell (HYPERFORGE-22).
//!
//! A stage's command runs under `sh -c` in the repo directory, in a
//! process group of its own, so a timeout or a cancel stops everything it
//! started. Its stdout and stderr are kept line by line and streamed.
//!
//! A *drive* is one piece of work on a run — a `start` or a `trigger` — and
//! the `auto` stages it sets off. It runs on its own task, so a caller
//! hanging up never stops a stage.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};

use super::events::{CiEvent, StageState};
use super::spec::{Declared, StageSpec, Trigger};
use super::store::CiStore;

/// How long a stage's process group gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long the log is drained after the command exits, for lines still
/// buffered (a background process holding the pipe is not waited for).
const DRAIN: Duration = Duration::from_secs(2);

/// An attempt already begun (by `begin_attempt`), to be run first.
pub struct Begun {
    pub stage: String,
    pub attempt: u32,
    pub cancel: Arc<Notify>,
    pub needed: Vec<(String, BTreeMap<String, String>)>,
}

fn env_name(s: &str) -> String {
    s.to_ascii_uppercase().replace('-', "_")
}

/// Parse `key=value` lines; blank lines and `#` comments are skipped, a
/// later key wins.
#[must_use]
pub fn parse_outputs(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .collect()
}

async fn kill_group(pid: u32, signal: &str) {
    let _ = tokio::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

enum Ended {
    Exited(Option<i32>),
    TimedOut(u64),
    Canceled,
    Spawn(String),
}

/// Run one begun attempt to its end, record it, and answer its state.
async fn run_attempt(
    store: &CiStore,
    decl: &Declared,
    run: u64,
    spec: &StageSpec,
    begun: Begun,
    tx: Option<&mpsc::UnboundedSender<CiEvent>>,
) -> StageState {
    let pipeline = decl.spec.name.clone();
    let Begun { stage, attempt, cancel, needed } = begun;
    store.emit(
        CiEvent::StageStarted { pipeline: pipeline.clone(), run, stage: stage.clone(), attempt },
        tx,
    );
    let outputs_path = store.outputs_path(&pipeline, run, &stage, attempt);
    let log_path = store.log_path(&pipeline, run, &stage, attempt);
    let _ = std::fs::write(&outputs_path, "");

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&spec.run)
        .current_dir(&decl.repo)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.envs(&decl.spec.env).envs(&spec.env);
    for (need, outputs) in &needed {
        for (k, v) in outputs {
            cmd.env(env_name(k), v);
            cmd.env(format!("HF_{}_{}", env_name(need), env_name(k)), v);
        }
    }
    cmd.env("HF_PIPELINE", &pipeline)
        .env("HF_RUN", run.to_string())
        .env("HF_STAGE", &stage)
        .env("HF_ATTEMPT", attempt.to_string())
        .env("HF_OUTPUTS", &outputs_path);

    let ended = match cmd.spawn() {
        Err(e) => Ended::Spawn(format!("could not start `sh -c`: {e}")),
        Ok(mut child) => {
            let pid = child.id().unwrap_or(0);
            let (ltx, mut lrx) = mpsc::unbounded_channel::<(&'static str, String)>();
            if let Some(out) = child.stdout.take() {
                let ltx = ltx.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(out).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        if ltx.send(("stdout", l)).is_err() {
                            break;
                        }
                    }
                });
            }
            if let Some(err) = child.stderr.take() {
                let ltx = ltx.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(err).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        if ltx.send(("stderr", l)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(ltx);
            let mut log = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .await
                .ok();
            let timeout = spec.timeout_minutes;
            let deadline = async {
                match timeout {
                    Some(m) => tokio::time::sleep(Duration::from_secs(m.saturating_mul(60))).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline);
            let write_line = |stream: &'static str, line: String| {
                let ev = CiEvent::Log {
                    pipeline: pipeline.clone(),
                    run,
                    stage: stage.clone(),
                    attempt,
                    stream: stream.to_string(),
                    line,
                };
                store.emit(ev, tx);
            };
            let mut pending_lines: Vec<String> = Vec::new();
            let ended = loop {
                tokio::select! {
                    Some((stream, line)) = lrx.recv() => {
                        pending_lines.push(format!("{line}\n"));
                        write_line(stream, line);
                    }
                    status = child.wait() => {
                        break Ended::Exited(status.ok().and_then(|s| s.code()));
                    }
                    () = &mut deadline => {
                        kill_group(pid, "TERM").await;
                        break Ended::TimedOut(timeout.unwrap_or(0));
                    }
                    () = cancel.notified() => {
                        kill_group(pid, "TERM").await;
                        break Ended::Canceled;
                    }
                }
                if let Some(f) = log.as_mut() {
                    for l in std::mem::take(&mut pending_lines) {
                        let _ = f.write_all(l.as_bytes()).await;
                    }
                }
            };
            if matches!(ended, Ended::TimedOut(_) | Ended::Canceled)
                && tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err()
            {
                kill_group(pid, "KILL").await;
                let _ = child.wait().await;
            }
            let drain_until = tokio::time::Instant::now() + DRAIN;
            while let Ok(Some((stream, line))) =
                tokio::time::timeout_at(drain_until, lrx.recv()).await
            {
                pending_lines.push(format!("{line}\n"));
                write_line(stream, line);
            }
            if let Some(f) = log.as_mut() {
                for l in std::mem::take(&mut pending_lines) {
                    let _ = f.write_all(l.as_bytes()).await;
                }
                let _ = f.flush().await;
            }
            ended
        }
    };

    let written = parse_outputs(&std::fs::read_to_string(&outputs_path).unwrap_or_default());
    let (state, exit_code, reason, outputs) = match ended {
        Ended::Exited(Some(0)) => {
            let missing: Vec<&String> =
                spec.outputs.iter().filter(|o| !written.contains_key(*o)).collect();
            if missing.is_empty() {
                (StageState::Succeeded, Some(0), None, written)
            } else {
                let names: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
                let reason = format!("exited 0 without writing output {}", names.join(", "));
                (StageState::Failed, Some(0), Some(reason), written)
            }
        }
        Ended::Exited(Some(code)) => {
            (StageState::Failed, Some(code), Some(format!("exit code {code}")), written)
        }
        Ended::Exited(None) => {
            (StageState::Failed, None, Some("killed by a signal".to_string()), written)
        }
        Ended::TimedOut(m) => {
            (StageState::Failed, None, Some(format!("timed out after {m} minutes")), written)
        }
        Ended::Canceled => (StageState::Canceled, None, Some("canceled".to_string()), written),
        Ended::Spawn(e) => (StageState::Failed, None, Some(e), BTreeMap::new()),
    };
    if let Err(e) = store.finish_attempt(
        &pipeline,
        run,
        &stage,
        attempt,
        state,
        exit_code,
        reason.clone(),
        outputs.clone(),
    ) {
        store.emit(CiEvent::error("store", e), tx);
    }
    store.emit(
        CiEvent::StageFinished {
            pipeline,
            run,
            stage,
            attempt,
            state,
            exit_code,
            reason,
            outputs,
        },
        tx,
    );
    state
}

/// Begin and run `spec` on `run`, answering whether it succeeded. A refusal
/// (it is running, a need is not met) is said as an error event.
async fn begin_and_run(
    store: &CiStore,
    decl: &Declared,
    run: u64,
    spec: &StageSpec,
    by: &str,
    tx: Option<&mpsc::UnboundedSender<CiEvent>>,
) -> bool {
    match store.begin_attempt(&decl.spec.name, run, &spec.name, &spec.needs, by) {
        Ok((attempt, cancel, needed)) => {
            let begun = Begun { stage: spec.name.clone(), attempt, cancel, needed };
            run_attempt(store, decl, run, spec, begun, tx).await == StageState::Succeeded
        }
        Err(e) => {
            store.emit(CiEvent::error("refused", e), tx);
            false
        }
    }
}

/// One drive on a run: a `trigger` or a `start`, and what it sets off.
///
/// First the begun attempt (a `trigger`) or, with none, the `auto` stages
/// that need nothing (a `start`); then, in the file's order,
/// every `auto` stage that needs a stage this drive saw succeed and whose
/// needs have all succeeded on the run. Each stage runs at most once per
/// drive. Ends with `idle` when no other drive works on the run.
pub async fn drive(
    store: Arc<CiStore>,
    decl: Declared,
    run: u64,
    first: Option<Begun>,
    by: String,
    tx: Option<mpsc::UnboundedSender<CiEvent>>,
) {
    let pipeline = decl.spec.name.clone();
    let tx = tx.as_ref();
    let mut ran: BTreeSet<String> = BTreeSet::new();
    let mut succeeded: BTreeSet<String> = BTreeSet::new();

    if let Some(begun) = first {
        let name = begun.stage.clone();
        ran.insert(name.clone());
        if let Some(spec) = decl.spec.stage(&name).cloned() {
            if run_attempt(&store, &decl, run, &spec, begun, tx).await == StageState::Succeeded {
                succeeded.insert(name);
            }
        }
    } else {
        for spec in decl
            .spec
            .stages
            .iter()
            .filter(|s| s.trigger == Trigger::Auto && s.needs.is_empty())
        {
            ran.insert(spec.name.clone());
            if begin_and_run(&store, &decl, run, spec, &by, tx).await {
                succeeded.insert(spec.name.clone());
            }
        }
    }

    loop {
        let rec = store.load(&pipeline, run);
        let met = |n: &String| {
            rec.as_ref()
                .and_then(|r| r.stage(n))
                .is_some_and(|s| s.state == StageState::Succeeded)
        };
        let next = decl.spec.stages.iter().find(|s| {
            s.trigger == Trigger::Auto
                && !ran.contains(&s.name)
                && s.needs.iter().any(|n| succeeded.contains(n))
                && s.needs.iter().all(met)
        });
        let Some(spec) = next.cloned() else { break };
        ran.insert(spec.name.clone());
        if begin_and_run(&store, &decl, run, &spec, &by, tx).await {
            succeeded.insert(spec.name.clone());
        }
    }

    if store.leave(&pipeline, run) {
        store.emit(CiEvent::Idle { pipeline, run }, tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_are_key_value_lines() {
        let o = parse_outputs("image=x\n\n# c\nrelease = r=1\r\nimage=y\n");
        assert_eq!(o["image"], "y");
        assert_eq!(o["release"], " r=1");
    }
}
