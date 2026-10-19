//! `build::exec` — subprocess runner used by `build.run` and
//! `build.exec`. Runs arbitrary shell commands inside a repo checkout
//! and captures stdout/stderr/exit code.

use std::path::Path;
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum ExecError {
    #[error("io error: {0}")]
    Io(String),
}

impl ExecError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "io"
    }
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run `cmd` as `sh -c <cmd>` inside `cwd`. Returns captured output
/// regardless of exit status; only true io errors (missing sh, cwd
/// unreadable, etc.) raise `ExecError`.
pub fn run_shell(cwd: &Path, cmd: &str) -> Result<ExecResult, ExecError> {
    let out = Command::new("sh")
        .current_dir(cwd)
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| ExecError::Io(e.to_string()))?;
    Ok(ExecResult {
        exit_code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// HYPERFORGE-23: [`run_shell`] with variables set in the child's
/// environment — how a secret reaches a command without appearing in its
/// command line.
pub fn run_shell_env(cwd: &Path, cmd: &str, envs: &[(&str, String)]) -> Result<ExecResult, ExecError> {
    let mut c = Command::new("sh");
    c.current_dir(cwd).arg("-c").arg(cmd);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().map_err(|e| ExecError::Io(e.to_string()))?;
    Ok(ExecResult {
        exit_code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}
