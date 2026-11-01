//! Hackage registry client.
//!
//! Queries Hackage API for published versions and shells out to
//! `cabal upload --publish` for publishing. Drift detection downloads
//! the published tarball and compares its SHA256 against a local
//! `cabal sdist` build.

use super::{DriftResult, PublishResult, PublishedVersion, RegistryClient};
use crate::auth::YamlAuthProvider;
use crate::auth::AuthProvider;
use crate::build_system::BuildSystemKind;
use crate::service::PackageRegistry;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
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
            .user_agent("hyperforge/4.0")
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

    /// Resolve the Hackage auth token. Tries in order:
    /// 1. `HACKAGE_TOKEN` env var
    /// 2. Secrets store (`hackage/token`)
    /// 3. `password-command` from `~/.cabal/config`
    async fn resolve_token(&self) -> Option<String> {
        let stored = match YamlAuthProvider::new() {
            Ok(provider) => provider.get_secret("hackage/token").await.ok().flatten(),
            Err(_) => None,
        };
        self.resolve_token_with(stored).await
    }

    /// [`Self::resolve_token`] with the caller's store: `stored` is what it
    /// holds at `hackage/token` (v5's flat `secrets.yaml` under its config
    /// dir, which `YamlAuthProvider` does not read).
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

#[async_trait]
impl RegistryClient for HackageClient {
    fn build_system(&self) -> BuildSystemKind {
        BuildSystemKind::Cabal
    }

    fn registry_kind(&self) -> PackageRegistry {
        PackageRegistry::Hackage
    }

    async fn published_version(&self, name: &str) -> anyhow::Result<Option<PublishedVersion>> {
        Ok(self.versions(name).await?.held.into_iter().next().map(|version| PublishedVersion {
            name: name.to_string(),
            version,
        }))
    }

    async fn publish(
        &self,
        path: &Path,
        name: &str,
        dry_run: bool,
    ) -> anyhow::Result<PublishResult> {
        if dry_run {
            // Dry-run: build source distribution to validate
            let output = Self::sdist_command(path, None).output().await?;

            let success = output.status.success();
            let error = if success {
                None
            } else {
                Some(String::from_utf8_lossy(&output.stderr).trim().to_string())
            };

            let version = crate::build_system::cabal::cabal_package_version(path)
                .unwrap_or_else(|| "unknown".to_string());

            return Ok(PublishResult {
                package_name: name.to_string(),
                version,
                success,
                error,
            });
        }

        // Resolve auth token before publishing
        let token = self.resolve_token().await;

        // Step 1: build source distribution tarball
        let sdist_output = Self::sdist_command(path, None).output().await?;

        if !sdist_output.status.success() {
            let version = crate::build_system::cabal::cabal_package_version(path)
                .unwrap_or_else(|| "unknown".to_string());
            return Ok(PublishResult {
                package_name: name.to_string(),
                version,
                success: false,
                error: Some(format!(
                    "cabal sdist failed: {}",
                    String::from_utf8_lossy(&sdist_output.stderr).trim()
                )),
            });
        }

        // Find the tarball path from sdist output (last line is the path)
        let sdist_stdout = String::from_utf8_lossy(&sdist_output.stdout);
        let tarball = sdist_stdout
            .lines()
            .rev()
            .find(|l| l.ends_with(".tar.gz"))
            .map(|l| l.trim().to_string());

        let tarball = if let Some(t) = tarball { t } else {
            let version = crate::build_system::cabal::cabal_package_version(path)
                .unwrap_or_else(|| "unknown".to_string());
            return Ok(PublishResult {
                package_name: name.to_string(),
                version,
                success: false,
                error: Some("cabal sdist did not produce a tarball path".to_string()),
            });
        };

        // Step 2: upload the tarball; the token bypasses the interactive
        // username prompt, from a private config, never the command line.
        let config = token.as_deref().map(Self::token_config).transpose()?;
        let output = Self::upload_command(path, &tarball, config.as_ref().map(tempfile::NamedTempFile::path))
            .output()
            .await?;

        let success = output.status.success();
        let error = if success {
            None
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(format!("{}\n{}", stderr.trim(), stdout.trim()).trim().to_string())
        };

        let version = crate::build_system::cabal::cabal_package_version(path)
            .unwrap_or_else(|| "unknown".to_string());

        Ok(PublishResult {
            package_name: name.to_string(),
            version,
            success,
            error,
        })
    }

    async fn detect_drift(
        &self,
        path: &Path,
        name: &str,
        version: &str,
    ) -> anyhow::Result<DriftResult> {
        // Step 1: Download published tarball from Hackage
        let url = format!(
            "https://hackage.haskell.org/package/{name}-{version}/{name}-{version}.tar.gz",
        );

        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Ok(DriftResult::Unknown);
        }
        let published_bytes = resp.bytes().await?;
        let published_hash = format!("{:x}", Sha256::digest(&published_bytes));

        // Step 2: Build local source distribution
        let output = tokio::process::Command::new("cabal")
            .args(["sdist", "--ignore-project"])
            .current_dir(path)
            .output()
            .await?;

        if !output.status.success() {
            return Ok(DriftResult::Unknown);
        }

        // Find the local tarball path (last .tar.gz line in stdout)
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tarball_path = match stdout.lines().rev().find(|l| l.ends_with(".tar.gz")) {
            Some(p) => p.trim().to_string(),
            None => return Ok(DriftResult::Unknown),
        };

        // Step 3: Hash the local tarball
        let local_bytes = match tokio::fs::read(&tarball_path).await {
            Ok(b) => b,
            Err(_) => return Ok(DriftResult::Unknown),
        };
        let local_hash = format!("{:x}", Sha256::digest(&local_bytes));

        // Step 4: If identical, done
        if local_hash == published_hash {
            return Ok(DriftResult::Identical);
        }

        // Step 5: Extract both and diff to find changed files
        let tmp_dir = std::env::temp_dir().join(format!(
            "hyperforge-drift-{}",
            uuid::Uuid::new_v4()
        ));
        let local_dir = tmp_dir.join("local");
        let published_dir = tmp_dir.join("published");

        let changed_files = async {
            tokio::fs::create_dir_all(&local_dir).await?;
            tokio::fs::create_dir_all(&published_dir).await?;

            let published_tar = tmp_dir.join("published.tar.gz");
            tokio::fs::write(&published_tar, &published_bytes).await?;

            tokio::process::Command::new("tar")
                .args(["xzf", &tarball_path])
                .current_dir(&local_dir)
                .output()
                .await?;
            tokio::process::Command::new("tar")
                .args(["xzf", &published_tar.display().to_string()])
                .current_dir(&published_dir)
                .output()
                .await?;

            let pkg_dir = format!("{name}-{version}");
            let output = tokio::process::Command::new("diff")
                .args([
                    "-rq",
                    &local_dir.join(&pkg_dir).display().to_string(),
                    &published_dir.join(&pkg_dir).display().to_string(),
                ])
                .output()
                .await?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let prefix = format!("{pkg_dir}/");
            let files: Vec<String> = stdout
                .lines()
                .filter_map(|line| {
                    if line.starts_with("Files ") && line.ends_with(" differ") {
                        let after_files = line.trim_start_matches("Files ").trim();
                        let local_path = after_files.split(" and ").next()?;
                        let pos = local_path.find(&prefix)?;
                        Some(local_path[pos + prefix.len()..].to_string())
                    } else if line.starts_with("Only in") {
                        Some(line.to_string())
                    } else {
                        None
                    }
                })
                .collect();

            Ok::<Vec<String>, anyhow::Error>(files)
        }
        .await;

        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

        Ok(DriftResult::Drifted {
            changed_files: changed_files.unwrap_or_default(),
        })
    }
}
