# CLEANUP-2: Fix plexus-macros handle_enum_tests (18 compile errors)

blocked_by: []
unlocks: [CLEANUP-8]
difficulty: trivial
cascades_to: [synapse, plexus-deployments]

## Problem

Incomplete rename refactor to `plexus_*` (commit 930581f, Feb 4).
Test file still imports `HandleEnum` from the macro crate's former name instead of `plexus_macros::HandleEnum`.

### Errors
- E0432: `unresolved import` of the former macro crate (line 4)
- E0599: `no method named 'to_handle'` (5x) — macro never applied
- E0599: `no method named 'resolution_params'` (3x) — macro never applied
- E0277: `TestHandle: TryFrom<&Handle>` unsatisfied (5x) — impl blocks never generated

## Fix

1. **`tests/handle_enum_tests.rs` line 4**: the former crate's `HandleEnum` import -> `use plexus_macros::HandleEnum`
2. **`Cargo.toml` dev-dependencies**: Add `plexus-macros = { path = ".." }`

### Bonus (latent bug, not causing failure)
- `src/handle_enum.rs` lines 354, 371: hardcoded `plexus_core::Handle::new()` should use `#crate_path::Handle::new()`
