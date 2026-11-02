//! `ops` — the v5 library layer (V5LIFECYCLE).
//!
//! All yaml I/O, `ForgePort` adapter calls, and `.hyperforge/config.toml`
//! filesystem interactions go through this module. Services (`ReposService`,
//! `WorkspacesService`, `OrgsService`, …) are translation layers: they call
//! `ops::*`, receive typed outcomes, and emit RPC event envelopes.
//! No service directly uses `serde_yaml`, `adapter.*`, `for_provider`, or
//! `std::fs` against config paths. D13 — enforced by V5LIFECYCLE-11's
//! grep-based checkpoint.

pub mod state;
pub mod pr;
pub mod release_assets;
pub mod repo;
pub mod fs;
pub mod git;
pub mod analytics;
pub mod external_auth;
