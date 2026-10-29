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

    /// Resolve the Hackage auth token. Tries in order:
    /// 1. `HACKAGE_TOKEN` env var
    /// 2. Secrets store (`hackage/token`)
    /// 3. `password-command` from `~/.cabal/config`
    async fn resolve_token(&self) -> Option<String> {
        // 1. Env var
        if let Ok(token) = std::env::var("HACKAGE_TOKEN") {
            if !token.is_empty() {
                return Some(token);
            }
        }

        // 2. Secrets store
        if let Ok(provider) = YamlAuthProvider::new() {
            if let Ok(Some(token)) = provider.get_secret("hackage/token").await {
                if !token.is_empty() {
                    return Some(token);
                }
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
        let url = format!(
            "https://hackage.haskell.org/package/{name}/preferred"
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let resp = resp.error_for_status()?;
        let body: serde_json::Value = resp.json().await?;

        // Hackage response: { "normal-version": ["1.2.3", "1.2.2", ...], ... }
        let version = body
            .get("normal-version")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        match version {
            Some(v) => Ok(Some(PublishedVersion {
                name: name.to_string(),
                version: v,
            })),
            None => Ok(None),
        }
    }

    async fn publish(
        &self,
        path: &Path,
        name: &str,
        dry_run: bool,
    ) -> anyhow::Result<PublishResult> {
        if dry_run {
            // Dry-run: build source distribution to validate
            let output = tokio::process::Command::new("cabal")
                .args(["sdist", "--ignore-project"])
                .current_dir(path)
                .output()
                .await?;

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
        let sdist_output = tokio::process::Command::new("cabal")
            .args(["sdist", "--ignore-project"])
            .current_dir(path)
            .output()
            .await?;

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

        // Step 2: upload the tarball with --token to bypass interactive username prompt
        let mut upload_args = vec!["upload".to_string(), "--publish".to_string()];
        if let Some(ref tok) = token {
            upload_args.push(format!("--token={tok}"));
        }
        upload_args.push(tarball);
        let output = tokio::process::Command::new("cabal")
            .args(&upload_args)
            .current_dir(path)
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
