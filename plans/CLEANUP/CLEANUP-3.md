# CLEANUP-3: Fix service-codegen rust_codegen_smoke_test (3 compile errors)

blocked_by: []
unlocks: [CLEANUP-8]
difficulty: moderate
cascades_to: [plexus-sandbox-ts, synapse-cc]

## Problem

Three separate issues from struct field additions that weren't propagated to tests:

### Error 1: E0432 — `generate_rust` not in scope
- `tests/rust_codegen_smoke_test.rs:3` imports `generate_rust`, but it's gated behind the `rust` feature
- Default features are `["typescript"]` only
- `cargo test` doesn't enable `rust` feature

### Error 2: E0063 — Missing `ir_backend` field in IR struct
- `tests/rust_codegen_smoke_test.rs:187` — `create_comprehensive_test_ir()` constructs `IR {}` without `ir_backend` field
- Field was added in commit 347c10ac (Feb 8)

### Error 3: E0063 — Missing `file_hashes` field in GenerationResult
- `src/generator/rust/mod.rs:72` — `GenerationResult { files, warnings }` missing `file_hashes`
- Field was added for cache invalidation support

### Error 4: E0425 — `generate_client` not found
- `src/generator/rust/tests.rs:195` calls `generate_client()` but function was renamed/removed

## Fix

1. **Run tests with `--all-features`** or add `rust` to test feature set
2. **`tests/rust_codegen_smoke_test.rs:187`**: Add `ir_backend: "test".to_string()` to IR construction
3. **`src/generator/rust/mod.rs:72`**: Add `file_hashes: HashMap::new()` to GenerationResult
4. **`src/generator/rust/tests.rs:195`**: Update function call to match current API
