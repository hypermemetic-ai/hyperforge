//! Hackage registry client.
//!
//! Queries Hackage for published versions and shells out to `cabal` for
//! checks, sdists and uploads.

use std::path::Path;

/// Every version Hackage holds of a package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HackageVersions {
    /// `normal-version` and `unpreferred-version`.
    pub held: Vec<String>,
    /// `deprecated-version`: still held, so never uploaded again.
    pub deprecated: Vec<String>,
}

/// Hackage registry client
pub struct HackageClient {
    http: reqwest::Client,
}

impl Default for HackageClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HackageClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("hyperforge/5.0")
            .build()
            .expect("failed to build HTTP client");
        Self { http }
    }

    /// Hackage, unless `HYPERFORGE_HACKAGE_URL` says otherwise (a `file://`
    /// directory is a Hackage on disk). Reads only: an upload goes where
    /// cabal's config says.
    fn base_url() -> String {
        std::env::var("HYPERFORGE_HACKAGE_URL")
            .unwrap_or_else(|_| "https://hackage.haskell.org".into())
            .trim_end_matches('/')
            .to_string()
    }

    /// The body at `url` (`file://` too); `None` when there is nothing there.
    async fn fetch(&self, url: &str, accept: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(path) = url.strip_prefix("file://") {
            return match std::fs::read(path) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            };
        }
        let resp = self.http.get(url).header("Accept", accept).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.bytes().await?.to_vec()))
    }

    /// Every version Hackage holds of `name` (none: never published).
    pub async fn versions(&self, name: &str) -> anyhow::Result<HackageVersions> {
        let url = format!("{}/package/{name}/preferred", Self::base_url());
        let Some(body) = self.fetch(&url, "application/json").await? else {
            return Ok(HackageVersions::default());
        };
        // Hackage response: { "normal-version": ["1.2.3", "1.2.2", ...], ... }
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        let list = |key: &str| -> Vec<String> {
            json.get(key)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let mut held = list("normal-version");
        held.extend(list("unpreferred-version"));
        Ok(HackageVersions { held, deprecated: list("deprecated-version") })
    }

    /// The published source tarball of `name` at `version`.
    pub async fn download(&self, name: &str, version: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let url = format!("{0}/package/{name}-{version}/{name}-{version}.tar.gz", Self::base_url());
        self.fetch(&url, "*/*").await
    }

    /// A `cabal` run in `dir`; a token in our environment is not passed on.
    fn cabal(dir: &Path) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("cabal");
        c.current_dir(dir).env_remove("HACKAGE_TOKEN");
        c
    }

    /// `cabal check`: exit 0 with warnings, non-zero on what Hackage rejects.
    #[must_use]
    pub fn check_command(dir: &Path) -> tokio::process::Command {
        let mut c = Self::cabal(dir);
        c.arg("check");
        c
    }

    /// `cabal sdist`, into `out` or cabal's default (`dist-newstyle/sdist`).
    #[must_use]
    pub fn sdist_command(dir: &Path, out: Option<&Path>) -> tokio::process::Command {
        let mut c = Self::cabal(dir);
        c.args(["sdist", "--ignore-project"]);
        if let Some(out) = out {
            c.arg("-o").arg(out);
        }
        c
    }

    /// `cabal upload --publish <tarball>`. The token is never an argument:
    /// `token_config` is a cabal config holding it ([`Self::token_config`]);
    /// without one, cabal uses its own credentials.
    #[must_use]
    pub fn upload_command(dir: &Path, tarball: &str, token_config: Option<&Path>) -> tokio::process::Command {
        let mut c = Self::cabal(dir);
        c.args(["upload", "--publish", tarball]);
        if let Some(config) = token_config {
            c.env("CABAL_CONFIG", config);
        }
        c
    }

    /// A cabal config holding `token`, for one upload: mode 0600, removed
    /// when dropped. `cabal upload` has no environment variable for a token;
    /// its choices are `--token` (the process table) or a config key.
    pub fn token_config(token: &str) -> std::io::Result<tempfile::NamedTempFile> {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new()?;
        write!(
            f,
            "repository hackage.haskell.org\n  url: https://hackage.haskell.org/\n\ntoken: {}\n",
            token.trim()
        )?;
        f.flush()?;
        Ok(f)
    }

    /// Resolve the Hackage auth token: `HACKAGE_TOKEN`, then `stored` (what
    /// the secret store holds at `hackage/token`), then `password-command`
    /// from `~/.cabal/config`.
    pub async fn resolve_token_with(&self, stored: Option<String>) -> Option<String> {
        // 1. Env var
        if let Ok(token) = std::env::var("HACKAGE_TOKEN") {
            if !token.is_empty() {
                return Some(token);
            }
        }

        // 2. Secrets store
        if let Some(token) = stored {
            if !token.is_empty() {
                return Some(token);
            }
        }

        // 3. Parse password-command from cabal config
        let config_path = dirs::home_dir()?.join(".cabal/config");
        let config = tokio::fs::read_to_string(&config_path).await.ok()?;
        let cmd = config
            .lines()
            .find(|l| l.starts_with("password-command:"))?
            .trim_start_matches("password-command:")
            .trim();

        if cmd.is_empty() {
            return None;
        }

        let output = tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .output()
            .await
            .ok()?;

        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }

        None
    }
}
