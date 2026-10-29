//! Registry shim — shells out to `synapse` CLI to interact with the Plexus registry.
//!
//! **TEMPORARY DESIGN**: This module uses subprocess calls to `synapse` as a stopgap.
//! It should be replaced with a native WebSocket JSON-RPC client once plexus-client
//! crate exists. Every temporary decision is annotated with a `// TEMP:` comment.

use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

/// The registry service's root namespace: the new name first, then the one
/// plexus-registry still serves by default (its `--namespace`), so this works
/// whichever lands first. The second goes when plexus-registry is renamed.
const REGISTRY_ROOTS: [&str; 2] = ["registry-service", "registry-hub"];

/// Configuration for connecting to the Plexus registry.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Port where the Plexus registry is listening.
    // TEMP: Hardcoded default 4444 — should come from service discovery.
    pub registry_port: u16,
    /// Name to register as (e.g. "lforge").
    pub name: String,
    /// Host the backend is reachable at.
    pub host: String,
    /// Port the backend is listening on.
    pub port: u16,
    /// Human-readable description.
    pub description: String,
    /// Plexus namespace for the backend.
    pub namespace: String,
}

impl RegistryConfig {
    pub fn new(name: impl Into<String>, port: u16) -> Self {
        Self {
            // TEMP: Hardcoded default — registry port should be discovered.
            registry_port: 4444,
            name: name.into(),
            host: "127.0.0.1".into(),
            port,
            description: "Multi-forge repository management".into(),
            namespace: "lforge".into(),
        }
    }
}

/// Parsed backend entry from `registry list` / `registry get`.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntry {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub namespace: Option<String>,
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// Errors from registry operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("synapse CLI not found on PATH — install synapse to enable registry integration")]
    SynapseNotFound,

    #[error("synapse command failed (exit {code}): {stderr}")]
    CommandFailed { code: i32, stderr: String },

    #[error("failed to parse synapse output: {0}")]
    ParseError(String),

    #[error("registry at port {0} is unreachable")]
    RegistryUnreachable(u16),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Client that shells out to `synapse` to call registry methods.
///
/// **TEMP**: This entire struct is a subprocess shim. Replace with a native
/// WebSocket JSON-RPC client when plexus-client crate is available.
#[derive(Debug, Clone)]
pub struct RegistryClient {
    config: RegistryConfig,
}

impl RegistryClient {
    pub const fn new(config: RegistryConfig) -> Self {
        Self { config }
    }

    /// Check whether `synapse` is available on PATH.
    pub async fn check_synapse_available() -> Result<(), RegistryError> {
        let output = Command::new("which")
            .arg("synapse")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        match output {
            Ok(status) if status.success() => Ok(()),
            _ => Err(RegistryError::SynapseNotFound),
        }
    }

    /// Run a synapse command against the registry backend and return raw stdout.
    ///
    /// TEMP: Subprocess shim — no reconnection or retry logic.
    async fn run_synapse(&self, args: &[&str]) -> Result<String, RegistryError> {
        let port_str = self.config.registry_port.to_string();
        // TEMP: Full path through the registry service's root → registry plugin.
        // Synapse only strips root namespace when it matches the first path segment,
        // and the registry service's root namespace is not "registry". It is
        // "registry-service" once plexus-registry takes the service naming, and
        // its old default until then (REGISTRY_ROOTS): the first that answers wins.
        let mut last = None;
        for root in REGISTRY_ROOTS {
            let mut cmd_args = vec!["-P", &port_str, "--json", root, "registry"];
            cmd_args.extend_from_slice(args);
            match self.run_synapse_at(&cmd_args).await {
                Ok(out) => return Ok(out),
                Err(e @ (RegistryError::SynapseNotFound | RegistryError::RegistryUnreachable(_))) => {
                    return Err(e)
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(RegistryError::SynapseNotFound))
    }

    async fn run_synapse_at(&self, cmd_args: &[&str]) -> Result<String, RegistryError> {
        tracing::debug!(
            "registry shim: synapse {}",
            cmd_args.join(" ")
        );

        let output = Command::new("synapse")
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RegistryError::SynapseNotFound
                } else {
                    RegistryError::IoError(e)
                }
            })?;

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);

            // Detect unreachable registry (connection refused in stderr)
            if stderr.contains("Connection refused") || stderr.contains("connect ECONNREFUSED") {
                return Err(RegistryError::RegistryUnreachable(self.config.registry_port));
            }

            return Err(RegistryError::CommandFailed { code, stderr });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Register this backend with the Plexus registry.
    ///
    /// If an entry with the same name already exists:
    /// - Check if the old backend is alive (TCP connect). If alive, abort.
    /// - If dead, delete the stale entry and register ourselves.
    pub async fn register(&self) -> Result<(), RegistryError> {
        // Check if an entry already exists with our name
        if let Ok(entry) = self.get(&self.config.name).await {
            let old_host = entry.host.as_deref().unwrap_or("127.0.0.1");
            let old_port = entry.port.unwrap_or(0);

            let needs_replace = old_host != self.config.host || old_port != self.config.port;

            if needs_replace {
                // TCP connect to old backend to see if it's actually alive
                let addr = format!("{old_host}:{old_port}");
                let old_alive = matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        tokio::net::TcpStream::connect(&addr),
                    ).await,
                    Ok(Ok(_))
                );

                if old_alive {
                    return Err(RegistryError::CommandFailed {
                        code: 1,
                        stderr: format!(
                            "another '{}' instance is already running at {}:{} — refusing to start",
                            self.config.name, old_host, old_port,
                        ),
                    });
                }

                // Old instance is dead — remove stale entry
                tracing::info!(
                    "old '{}' at {}:{} is unreachable — replacing stale registration",
                    self.config.name, old_host, old_port,
                );
                let _ = self.deregister().await;
            }
        }

        // Register ourselves
        let port_str = self.config.port.to_string();
        self.run_synapse(&[
            "register",
            "--name", &self.config.name,
            "--host", &self.config.host,
            "--port", &port_str,
            "--description", &self.config.description,
            "--namespace", &self.config.namespace,
        ]).await?;

        tracing::info!(
            "registered '{}' with registry at port {}",
            self.config.name,
            self.config.registry_port,
        );
        Ok(())
    }

    /// Deregister this backend from the Plexus registry.
    pub async fn deregister(&self) -> Result<(), RegistryError> {
        self.run_synapse(&["delete", "--name", &self.config.name]).await?;

        tracing::info!(
            "deregistered '{}' from registry at port {}",
            self.config.name,
            self.config.registry_port,
        );

        Ok(())
    }

    /// List all backends registered with the registry.
    pub async fn list(&self) -> Result<Vec<RegistryEntry>, RegistryError> {
        let stdout = self.run_synapse(&["list"]).await?;
        parse_json_lines(&stdout)
    }

    /// Get info for a specific backend by name.
    pub async fn get(&self, name: &str) -> Result<RegistryEntry, RegistryError> {
        let stdout = self.run_synapse(&["get", "--name", name]).await?;
        let entries: Vec<RegistryEntry> = parse_json_lines(&stdout)?;
        entries
            .into_iter()
            .next()
            .ok_or_else(|| RegistryError::ParseError(format!("no entry returned for '{name}'")))
    }

    /// Ping a backend by name through the registry.
    pub async fn ping(&self, name: &str) -> Result<RegistryEntry, RegistryError> {
        let stdout = self.run_synapse(&["ping", "--name", name]).await?;
        let entries: Vec<RegistryEntry> = parse_json_lines(&stdout)?;
        entries
            .into_iter()
            .next()
            .ok_or_else(|| RegistryError::ParseError(format!("no ping response for '{name}'")))
    }
}

/// Parse newline-delimited JSON from synapse --json output.
fn parse_json_lines<T: serde::de::DeserializeOwned>(raw: &str) -> Result<Vec<T>, RegistryError> {
    let mut results = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: T = serde_json::from_str(trimmed)
            .map_err(|e| RegistryError::ParseError(format!("{e}: {trimmed}")))?;
        results.push(entry);
    }
    Ok(results)
}
