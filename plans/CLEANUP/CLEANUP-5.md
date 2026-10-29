# CLEANUP-5: Fix plexus-registry doctest (1 failure)

blocked_by: []
unlocks: [CLEANUP-8]
difficulty: trivial
cascades_to: []

## Problem

Doctest at `src/lib.rs:15` has two stale imports from the `service_*` -> `plexus_*` rename:

1. `use registry::{Registry, RegistryStorageConfig}` — should be `use plexus_registry::`
2. `use plexus_core::plexus::Plexus` — `Plexus` type doesn't exist in plexus-core 0.3.0

### Errors
- E0432: unresolved import `registry`
- E0432: `no Plexus in plexus`
- E0282: type annotations needed (cascade)

## Fix

Update the doctest in `src/lib.rs` (~line 15):
- Change `use registry::` to `use plexus_registry::`
- Remove `use plexus_core::plexus::Plexus` and the `Plexus::new()` call
- Or simplify to `no_run`/`ignore` if a working example is too complex for a doctest
