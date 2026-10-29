# CLEANUP-6: Fix plexus-substrate test_plugin_schema_with_return_types (1 assertion failure)

blocked_by: []
unlocks: [CLEANUP-8]
difficulty: trivial
cascades_to: []

## Problem

Test at `src/activations/bash/activation.rs:97`:
```rust
assert_eq!(schema.methods.len(), 1);
```
Fails with `left: 2, right: 1`.

The `register_default_templates()` method (added in commit 85377df1) is now being picked up by the `#[activation]` macro and included in the schema, so 2 methods appear instead of the expected 1.

## Fix

**Option A (simplest):** Update the assertion to expect 2 methods:
```rust
assert_eq!(schema.methods.len(), 2);
```

**Option B (if register_default_templates shouldn't be in schema):** Move `register_default_templates` out of the `#[activation]` impl block into a separate impl block, or remove its `#[method]` attribute if one was accidentally added.
