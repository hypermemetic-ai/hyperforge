//! Pipelines on hyperforge (HYPERFORGE-22).
//!
//! A repo declares pipelines in `.hyperforge/pipelines.toml` ([`spec`]);
//! `ci add` registers it ([`registry`], kept in the config dir); runs,
//! attempts, outputs and logs are kept under the state dir ([`store`]);
//! stages run under the local shell ([`runner`]). The wire surface is
//! [`service::CiService`], a child of the v5 root, with each pipeline a dynamic
//! child of it.

pub mod events;
pub mod service;
pub mod registry;
pub mod runner;
pub mod runners;
pub mod spec;
pub mod store;

pub use service::{CiService, PipelineService};
pub use store::CiStore;
