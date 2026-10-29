//! Package registry clients for version querying and publishing.
//!
//! Provides a unified trait for interacting with package registries
//! (crates.io, Hackage, npm) and concrete implementations.

pub mod crates_io;
pub mod hackage;

use crate::build_system::BuildSystemKind;
use crate::service::PackageRegistry;
use async_trait::async_trait;
use std::path::Path;

/// Version information from a registry
#[derive(Debug, Clone)]
pub struct PublishedVersion {
    pub name: String,
    pub version: String,
}

/// Result of a publish operation
#[derive(Debug, Clone)]
pub struct PublishResult {
    pub package_name: String,
    pub version: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Result of a drift detection check.
///
/// Each registry implements detection using the most reliable mechanism
/// available (checksum comparison, tarball diff, etc.). Callers don't
/// need to know the method — just the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftResult {
    /// Package content is identical to what's published.
    Identical,
    /// Package content differs from what's published.
    /// `changed_files` lists the files that differ (may be empty if
    /// the registry detected drift but couldn't enumerate files).
    Drifted { changed_files: Vec<String> },
    /// Could not determine (registry doesn't support comparison).
    Unknown,
}

/// Unified interface for package registry operations
#[async_trait]
pub trait RegistryClient: Send + Sync {
    /// Which build system this registry serves
    fn build_system(&self) -> BuildSystemKind;

    /// Registry identifier for event reporting
    fn registry_kind(&self) -> PackageRegistry;

    /// Query the registry for the latest published version.
    /// Returns None if the package has never been published.
    async fn published_version(&self, name: &str) -> anyhow::Result<Option<PublishedVersion>>;

    /// Publish a package. If `dry_run` is true, validate without actually publishing.
    async fn publish(
        &self,
        path: &Path,
        name: &str,
        dry_run: bool,
    ) -> anyhow::Result<PublishResult>;

    /// Detect whether local package content differs from what's published.
    ///
    /// Each registry uses the most reliable mechanism available:
    /// - crates.io: SHA256 checksum comparison (cargo package vs registry)
    /// - Hackage: tarball hash comparison (cabal sdist vs published)
    ///
    /// Default returns `Unknown` for registries that haven't implemented this.
    async fn detect_drift(
        &self,
        _path: &Path,
        _name: &str,
        _version: &str,
    ) -> anyhow::Result<DriftResult> {
        Ok(DriftResult::Unknown)
    }
}

/// Get the appropriate registry client for a build system kind.
/// Returns None for Unknown or unsupported build systems.
pub fn registry_for(kind: &BuildSystemKind) -> Option<Box<dyn RegistryClient>> {
    match kind {
        BuildSystemKind::Cargo => Some(Box::new(crates_io::CratesIoClient::new())),
        BuildSystemKind::Cabal => Some(Box::new(hackage::HackageClient::new())),
        BuildSystemKind::Node | BuildSystemKind::Unknown => None,
    }
}
