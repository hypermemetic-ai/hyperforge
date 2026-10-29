//! Hyperforge v5 — ground-up rewrite living alongside v4.
//!
//! Design invariants and epic docs live under `plans/v5/`. This module
//! holds the v5 activation tree, config loaders, and secret store. The
//! v4 activation tree in `crate::service` / `crate::services` is untouched.

pub mod adapters;
pub mod build;
pub mod build_service;
pub mod ci;
pub mod config;
pub mod service;
pub mod ops;
pub mod orgs;
pub mod repos;
pub mod secrets;
pub mod secrets_service;
pub mod workspaces;
