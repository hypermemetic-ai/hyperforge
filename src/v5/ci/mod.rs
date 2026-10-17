//! Pipelines on hyperforge (HYPERFORGE-22).
//!
//! A repo declares pipelines in `.hyperforge/pipelines.toml` ([`spec`]);
//! `ci add` registers it ([`registry`], kept in the config dir); runs,
//! attempts, outputs and logs are kept under the state dir ([`store`]);
//! stages run under the local shell ([`runner`]). The wire surface is
//! [`hub::CiHub`], a child of the v5 root, with each pipeline a dynamic
//! child of it.

pub mod events;
pub mod hub;
pub mod registry;
pub mod runner;
pub mod spec;
pub mod store;

pub use hub::{CiHub, PipelineHub};
pub use store::CiStore;
