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

/// The npmrc line that makes npm read `NPM_TOKEN`; npm ignores that variable
/// without one. It names the variable and holds no secret.
pub const NPM_AUTH_LINE: &str = "//registry.npmjs.org/:_authToken=${NPM_TOKEN}\n";

/// HYPERFORGE-46: a throwaway npm user config, for `NPM_CONFIG_USERCONFIG`.
/// Owner-only, and removed when dropped.
pub fn npm_auth_file() -> Result<tempfile::NamedTempFile, ExecError> {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix("hyperforge-npmrc-")
        .tempfile()
        .map_err(|e| ExecError::Io(e.to_string()))?;
    file.write_all(NPM_AUTH_LINE.as_bytes()).map_err(|e| ExecError::Io(e.to_string()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_npm_auth_file_names_the_token_and_holds_none() {
        use std::os::unix::fs::PermissionsExt as _;
        let file = npm_auth_file().unwrap();
        let path = file.path().to_path_buf();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("${NPM_TOKEN}"));
        assert!(!body.contains("not-a-real-token"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let cmd = r#"grep -qF '${NPM_TOKEN}' "$NPM_CONFIG_USERCONFIG" && [ "$NPM_TOKEN" = not-a-real-token ]"#;
        let out = run_shell_env(
            Path::new("."),
            cmd,
            &[("NPM_TOKEN", "not-a-real-token".into()), ("NPM_CONFIG_USERCONFIG", path.display().to_string())],
        )
        .unwrap();
        assert_eq!(out.exit_code, 0, "{}", out.stderr);
        drop(file);
        assert!(!path.exists());
    }
}
