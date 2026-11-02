//! `[ci]` runners in a repo's `.hyperforge/config.toml` (HYPERFORGE-57).
//!
//! The layered runners v4's `build run --level` executed, read where they
//! already are: `[ci]` with `skip_validate` and an ordered `[[ci.runners]]`
//! list, index 0 the quickest (by convention 0 quick check, 1 full build,
//! 2 containerized, higher ones custom gates such as PERF-6). Each runner
//! has a `build` and a `test` command (argument lists joined with spaces and
//! run under `sh -c` in the repo, or in `image` with the repo mounted at
//! `/workspace` for `type = "docker"`), `env`, and `timeout_secs`.
//!
//! Decision (HYPERFORGE-57): `ci run` reads `[ci]` directly rather than
//! migrating 46 configs into `pipelines.toml`; pipelines stay the surface for
//! staged, triggered, recorded runs (deploys), runners for "does it build and
//! pass".

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

/// Lines of a failed step's output carried in its event.
pub const TAIL_LINES: usize = 40;

/// Where a runner runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Local,
    Docker,
}

/// One `[[ci.runners]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunnerConfig {
    #[serde(rename = "type")]
    pub kind: RunnerKind,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default)]
    pub test: Vec<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

const fn default_timeout() -> u64 {
    300
}

/// A repo's `[ci]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CiSection {
    #[serde(default)]
    pub skip_validate: bool,
    #[serde(default)]
    pub runners: Vec<RunnerConfig>,
}

/// Read `[ci]` from `<dir>/.hyperforge/config.toml`. `Ok(None)` when there
/// is no file or no `[ci]` table; `Err` naming the file when either does not
/// parse. Other tables in the file are not this module's and are ignored.
pub fn read(dir: &Path) -> Result<Option<CiSection>, String> {
    let path = dir.join(".hyperforge").join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let value: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("{}: {}", path.display(), e.to_string().trim_end()))?;
    let Some(ci) = value.get("ci") else { return Ok(None) };
    ci.clone()
        .try_into::<CiSection>()
        .map(Some)
        .map_err(|e| format!("{}: [ci]: {}", path.display(), e.to_string().trim_end()))
}

/// The runner indices a run executes: `level` alone when given (an index
/// past the list is `None`, for the caller to refuse or skip), else every
/// local runner, as v4 did.
#[must_use]
pub fn select(runners: &[RunnerConfig], level: Option<usize>) -> Option<Vec<usize>> {
    match level {
        Some(l) if l < runners.len() => Some(vec![l]),
        Some(_) => None,
        None => Some(
            runners
                .iter()
                .enumerate()
                .filter(|(_, r)| r.kind == RunnerKind::Local)
                .map(|(i, _)| i)
                .collect(),
        ),
    }
}

/// What one build or test step came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub ok: bool,
    pub exit_code: Option<i32>,
    /// The last [`TAIL_LINES`] lines of stdout then stderr.
    pub tail: String,
    /// Why it did not run to an exit (spawn failure, timeout).
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// The program and arguments a step runs as: `sh -c <cmd>` in the repo, or
/// `<docker> run --rm -v <repo>:/workspace -w /workspace [-e K=V]… <image> sh -c <cmd>`.
#[must_use]
pub fn argv(cmd: &[String], dir: &Path, runner: &RunnerConfig, docker: &str) -> (String, Vec<String>) {
    let shell = cmd.join(" ");
    match runner.kind {
        RunnerKind::Local => ("sh".into(), vec!["-c".into(), shell]),
        RunnerKind::Docker => {
            let mut args = vec![
                "run".into(),
                "--rm".into(),
                "-v".into(),
                format!("{}:/workspace", dir.display()),
                "-w".into(),
                "/workspace".into(),
            ];
            for (k, v) in &runner.env {
                args.push("-e".into());
                args.push(format!("{k}={v}"));
            }
            args.push(runner.image.clone().unwrap_or_else(|| "rust:latest".into()));
            args.extend(["sh".into(), "-c".into(), shell]);
            (docker.into(), args)
        }
    }
}

fn tail(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!("{}{}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: Option<R>) -> Vec<u8> {
    let mut b = Vec::new();
    if let Some(mut r) = r {
        let _ = r.read_to_end(&mut b).await;
    }
    b
}

/// Run one step to its exit or its runner's timeout. The step runs in its own
/// process group; on timeout the whole group is killed, so a `cargo` under
/// `sh` does not outlive it.
pub async fn execute(cmd: &[String], dir: &Path, runner: &RunnerConfig, docker: &str) -> StepOutcome {
    let started = Instant::now();
    let (program, args) = argv(cmd, dir, runner, docker);
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if runner.kind == RunnerKind::Local {
        command.envs(&runner.env);
    }
    let done = |ok, exit_code, tail, error: Option<String>| StepOutcome {
        ok,
        exit_code,
        tail,
        error,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return done(false, None, String::new(), Some(format!("could not start {program}: {e}"))),
    };
    let pid = child.id();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let waited = tokio::time::timeout(Duration::from_secs(runner.timeout_secs), async {
        tokio::join!(read_all(out), read_all(err), child.wait())
    })
    .await;
    match waited {
        Ok((o, e, Ok(status))) => done(status.success(), status.code(), tail(&o, &e), None),
        Ok((o, e, Err(err))) => done(false, None, tail(&o, &e), Some(format!("waiting for {program}: {err}"))),
        Err(_) => {
            if let Some(pid) = pid {
                // The group's id is the child's pid (process_group(0)).
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &format!("-{pid}")])
                    .status();
            }
            done(false, None, String::new(), Some(format!("timed out after {}s", runner.timeout_secs)))
        }
    }
}

/// One row of a run's result table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoResult {
    pub repo: String,
    /// `passed`, `failed`, `skipped` or `dry_run`.
    pub status: String,
    pub steps: u32,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The result table: one line per repo.
#[must_use]
pub fn table(rows: &[RepoResult]) -> String {
    let header = ["REPO", "RESULT", "STEPS", "TIME", "DETAIL"].map(String::from);
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.repo.clone(),
                r.status.clone(),
                r.steps.to_string(),
                format!("{}.{}s", r.duration_ms / 1000, (r.duration_ms % 1000) / 100),
                r.detail.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let width = |c: usize| cells.iter().chain([&header]).map(|r| r[c].chars().count()).max().unwrap_or(0);
    let w = [width(0), width(1), width(2), width(3)];
    let line = |r: &[String; 5]| {
        let mut l = format!(
            "{:w0$}  {:w1$}  {:w2$}  {:w3$}  {}",
            r[0], r[1], r[2], r[3], r[4],
            w0 = w[0], w1 = w[1], w2 = w[2], w3 = w[3]
        )
        .trim_end()
        .to_string();
        l.push('\n');
        l
    };
    let mut out = line(&header);
    for r in &cells {
        out.push_str(&line(r));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn repo(config: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".hyperforge")).unwrap();
        std::fs::write(d.path().join(".hyperforge/config.toml"), config).unwrap();
        d
    }

    const LAYERED: &str = r#"
repo_name = "x"
org = "o"

[ci]
skip_validate = false

[[ci.runners]]
type = "local"
build = ["echo", "quick"]
test = ["true"]

[[ci.runners]]
type = "docker"
image = "img:1"
build = ["cargo", "build"]
env = { A = "1" }

[[ci.runners]]
type = "local"
build = ["exit", "3"]
timeout_secs = 5
"#;

    #[test]
    fn reads_the_ci_table_and_ignores_the_rest() {
        let d = repo(LAYERED);
        let ci = read(d.path()).unwrap().unwrap();
        assert_eq!(ci.runners.len(), 3);
        assert_eq!(ci.runners[0].timeout_secs, 300);
        assert_eq!(ci.runners[1].kind, RunnerKind::Docker);
        assert_eq!(ci.runners[2].timeout_secs, 5);
    }

    #[test]
    fn no_file_or_no_table_is_none_and_a_bad_table_names_the_file() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read(empty.path()).unwrap(), None);
        assert_eq!(read(repo("org = \"o\"\n").path()).unwrap(), None);
        let bad = repo("[ci]\nrunners = [{ type = \"remote\" }]\n");
        let e = read(bad.path()).unwrap_err();
        assert!(e.contains("config.toml") && e.contains("[ci]"), "{e}");
    }

    #[test]
    fn selects_a_level_or_every_local_runner() {
        let ci = read(repo(LAYERED).path()).unwrap().unwrap();
        assert_eq!(select(&ci.runners, None), Some(vec![0, 2]));
        assert_eq!(select(&ci.runners, Some(1)), Some(vec![1]));
        assert_eq!(select(&ci.runners, Some(3)), None);
    }

    #[test]
    fn a_docker_runner_mounts_the_repo_and_passes_env() {
        let ci = read(repo(LAYERED).path()).unwrap().unwrap();
        let (p, a) = argv(&ci.runners[1].build, Path::new("/r"), &ci.runners[1], "docker");
        assert_eq!(p, "docker");
        assert_eq!(
            a,
            ["run", "--rm", "-v", "/r:/workspace", "-w", "/workspace", "-e", "A=1", "img:1", "sh", "-c", "cargo build"]
        );
    }

    #[tokio::test]
    async fn a_local_step_passes_fails_and_times_out() {
        let d = repo(LAYERED);
        let ci = read(d.path()).unwrap().unwrap();
        let ok = execute(&ci.runners[0].build, d.path(), &ci.runners[0], "docker").await;
        assert!(ok.ok && ok.exit_code == Some(0) && ok.tail == "quick", "{ok:?}");
        let bad = execute(&ci.runners[2].build, d.path(), &ci.runners[2], "docker").await;
        assert_eq!((bad.ok, bad.exit_code), (false, Some(3)), "{bad:?}");
        let slow = RunnerConfig { timeout_secs: 1, ..ci.runners[0].clone() };
        let t = execute(&["sleep".into(), "30".into()], d.path(), &slow, "docker").await;
        assert!(!t.ok && t.error.as_deref() == Some("timed out after 1s") && t.duration_ms < 10_000, "{t:?}");
    }

    #[tokio::test]
    async fn a_docker_step_runs_through_the_docker_program() {
        let d = repo(LAYERED);
        let ci = read(d.path()).unwrap().unwrap();
        // A fake docker: prints its argv, succeeds.
        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("docker");
        std::fs::write(&fake, "#!/bin/sh\necho \"$@\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = execute(&ci.runners[1].build, d.path(), &ci.runners[1], fake.to_str().unwrap()).await;
        assert!(out.ok, "{out:?}");
        assert!(out.tail.ends_with("img:1 sh -c cargo build"), "{}", out.tail);
        let missing = execute(&ci.runners[1].build, d.path(), &ci.runners[1], "/nonexistent/docker").await;
        assert!(!missing.ok && missing.error.unwrap().contains("could not start"));
    }

    #[test]
    fn the_table_has_a_row_per_repo() {
        let t = table(&[
            RepoResult { repo: "a".into(), status: "passed".into(), steps: 2, duration_ms: 1500, detail: None },
            RepoResult {
                repo: "bb".into(),
                status: "failed".into(),
                steps: 1,
                duration_ms: 0,
                detail: Some("L1 build exit 3".into()),
            },
        ]);
        assert_eq!(
            t,
            "REPO  RESULT  STEPS  TIME  DETAIL\na     passed  2      1.5s\nbb    failed  1      0.0s  L1 build exit 3\n"
        );
    }
}
