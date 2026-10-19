//! `build` — V5PARITY-9/10/11 build cluster.
//!
//! Pure-function helpers for manifest inspection, version bumps,
//! distribution config, and subprocess execution. Per D13, every
//! subprocess / git / filesystem op goes through `ops::*`; this layer
//! adds build-specific parsing + templating on top.

pub mod manifest;
pub mod diff;
pub mod release;
pub mod exec;
pub mod dist;
// HYPERFORGE-23: a crate chain across repos — diff, bump, publish.
pub mod chain;
pub mod chain_ops;
