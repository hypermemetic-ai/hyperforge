//! Hyperforge - Multi-forge repository management
//!
//! Hyperforge manages repositories across multiple git forges (GitHub, Codeberg, GitLab)
//! using declarative configuration and git as the source of truth.

pub mod adapters;
pub mod auth;
pub mod auth_service;
pub mod build_system;
pub mod commands;
pub mod config;
pub mod docker;
pub mod git;
pub mod service;
pub mod package;
pub mod registry;
pub mod remote;
pub mod services;
pub mod types;
pub mod v5;

// Re-export serde_helpers from plexus_core (required by plexus_macros generated code)
pub use plexus_core::serde_helpers;

// Re-exports for convenience
pub use adapters::{ForgePort, LocalForge};
pub use auth_service::{AuthEvent, AuthService};
pub use config::HyperforgeConfig;
pub use git::Transport;
pub use service::{
    HyperforgeEvent, HyperforgeService, OrgAddFailureReason, OrgUpdateFailureReason, OrgUpdateOp,
    TransportChangeFailureReason,
};
pub use services::{SymmetricSyncService, SyncDiff, SyncOp};
pub use types::*;
