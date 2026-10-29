//! crates.io registry client.
//!
//! Queries crates.io API for published versions and shells out to
//! `cargo publish` for publishing. Drift detection uses SHA256
//! checksum comparison, then extracts and diffs artifacts to report
//! exactly which files changed.

use super::{DriftResult, PublishResult, PublishedVersion, RegistryClient};
use crate::build_system::BuildSystemKind;
use crate::service::PackageRegistry;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::Path;

/// crates.io registry client
pub struct CratesIoClient {
    http: reqwest::Client,
}

impl Default for CratesIoClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CratesIoClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("hyperforge/4.0 (https://github.com/juggernautlabs/hyperforge)")
            .build()
            .expect("failed to build HTTP client");
        Self { http }
    }

    /// Fetch the SHA256 checksum for a specific version from crates.io.
    async fn published_checksum(&self, name: &str, version: &str) -> anyhow::Result<Option<String>> {
        let url = format!("https://crates.io/api/v1/crates/{name}/{version}");
        let resp = self.http.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let resp = resp.error_for_status()?;
        let body: serde_json::Value = resp.json().await?;

        let checksum = body
            .get("version")
            .and_then(|v| v.get("checksum"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        Ok(checksum)
    }

    /// Download a published .crate, extract both it and the local one,
    /// and return the list of files that differ.
    async fn diff_crate_contents(
        &self,
        local_crate: &Path,
        name: &str,
        version: &str,
    ) -> Vec<String> {
        let tmp_dir = std::env::temp_dir().join(format!(
            "hyperforge-drift-{}",
            uuid::Uuid::new_v4()
        ));
        let local_dir = tmp_dir.join("local");
        let published_dir = tmp_dir.join("published");

        let result = async {
            tokio::fs::create_dir_all(&local_dir).await?;
            tokio::fs::create_dir_all(&published_dir).await?;

            // Download published .crate
            let url = format!(
                "https://static.crates.io/crates/{name}/{name}-{version}.crate"
            );
            let resp = self.http.get(&url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("download failed");
            }
            let published_crate = tmp_dir.join("published.crate");
            tokio::fs::write(&published_crate, resp.bytes().await?).await?;

            // Extract both
            tokio::process::Command::new("tar")
                .args(["xzf", &local_crate.display().to_string()])
                .current_dir(&local_dir)
                .output()
                .await?;
            tokio::process::Command::new("tar")
                .args(["xzf", &published_crate.display().to_string()])
                .current_dir(&published_dir)
                .output()
                .await?;

            // Diff
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
                    // "Files .../local/pkg-0.1.0/Cargo.lock and .../published/pkg-0.1.0/Cargo.lock differ"
                    if line.starts_with("Files ") && line.ends_with(" differ") {
                        // Extract relative path from the local side
                        let after_files = line.trim_start_matches("Files ").trim();
                        let local_path = after_files.split(" and ").next()?;
                        let pos = local_path.find(&prefix)?;
                        Some(local_path[pos + prefix.len()..].to_string())
                    } else if line.starts_with("Only in") {
                        // "Only in .../local/pkg-0.1.0/src: new_file.rs"
                        Some(line.to_string())
                    } else {
                        None
                    }
                })
                .collect();

            Ok::<Vec<String>, anyhow::Error>(files)
        }
        .await;

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

        result.unwrap_or_default()
    }
}

#[async_trait]
impl RegistryClient for CratesIoClient {
    fn build_system(&self) -> BuildSystemKind {
        BuildSystemKind::Cargo
    }

    fn registry_kind(&self) -> PackageRegistry {
        PackageRegistry::CratesIo
    }

    async fn published_version(&self, name: &str) -> anyhow::Result<Option<PublishedVersion>> {
        let url = format!("https://crates.io/api/v1/crates/{name}");

        let resp = self.http.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let resp = resp.error_for_status()?;
        let body: serde_json::Value = resp.json().await?;

        let version = body
            .get("crate")
            .and_then(|c| c.get("max_stable_version"))
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
        let mut args = vec!["publish", "--no-verify"];
        if dry_run {
            args.push("--dry-run");
        }

        let output = tokio::process::Command::new("cargo")
            .args(&args)
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

        let version = crate::build_system::cargo::cargo_package_version(path)
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
        // Step 1: Get published checksum from crates.io
        let published_hash = match self.published_checksum(name, version).await? {
            Some(h) => h,
            None => return Ok(DriftResult::Unknown),
        };

        // Step 2: Build local .crate file (no verify = skip compilation)
        let output = tokio::process::Command::new("cargo")
            .args(["package", "--no-verify", "--allow-dirty"])
            .current_dir(path)
            .output()
            .await?;

        if !output.status.success() {
            return Ok(DriftResult::Unknown);
        }

        // Step 3: Find and hash the local .crate file
        let crate_file = path
            .join("target")
            .join("package")
            .join(format!("{name}-{version}.crate"));

        let bytes = match tokio::fs::read(&crate_file).await {
            Ok(b) => b,
            Err(_) => return Ok(DriftResult::Unknown),
        };

        let local_hash = format!("{:x}", Sha256::digest(&bytes));

        // Step 4: If identical, done
        if local_hash == published_hash {
            return Ok(DriftResult::Identical);
        }

        // Step 5: Download published .crate, extract both, diff
        let changed_files = self.diff_crate_contents(&crate_file, name, version).await;

        Ok(DriftResult::Drifted { changed_files })
    }
}
