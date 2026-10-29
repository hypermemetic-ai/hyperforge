---
id: V5PARITY-17
title: "TYPED-RETURNS — per-method return types for fluent-client bindings"
status: Pending
type: implementation
blocked_by: [V5PARITY-14]
unlocks: []
---

## Problem

Every service today returns a single grab-bag enum (`WorkspacesEvent`, `RepoEvent`, `OrgsEvent`, `BuildEvent`, `SecretsEvent`, `HyperforgeV5Event`). `workspaces.clone` and `workspaces.tag` both return `Stream<Item = WorkspacesEvent>` — the type system can't distinguish them, and a fluent client generated from the schema can't either: every workspace method binds to the same union, so calling `client.workspaces.clone()` returns a stream typed against every variant any workspace method might emit.

This is wrong in both directions:
- **At the type level:** `workspaces.clone` cannot emit `StatusSnapshot`, but the type permits it. Consumers pattern-match on variants that are statically unreachable.
- **At the wire level:** plexus generates fluent client bindings off the `Stream<Item = T>` generic. A grab-bag T means generic clients; per-method T means each call binds to the exact event surface the method emits.

## Required behavior

**Each `#[plexus_macros::method]` returns its own typed event enum.** Naming convention: `<Method>Event` (e.g., `CloneEvent`, `TagEvent`, `StatusEvent`). Variants are exactly what the method emits — no nullable-by-convention fields, no "this variant only appears for method X" comments.

**Shared variant payloads** (`MemberGitResult`, `WorkspaceGitSummary`, `Error`, `RepoRefWire`, etc.) stay shared as **struct types**, not enum variants. Each per-method enum names them as wrapped variants:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CloneEvent {
    MemberGitResult(MemberGitResult),     // shared payload struct
    WorkspaceGitSummary(WorkspaceGitSummary),
    Error(Error),
}
```

The wire shape stays JSON `{"type": "member_git_result", ...}` because of `serde(tag)`. Implementer is free to use a small helper macro to cut declaration boilerplate.

**Schema impact.** Each method now contributes its own JSON Schema entry. Plexus's fluent-client generator binds each method's `Stream<Item = T>` to its specific T. Downstream consumers get sharp types.

**Backwards-incompatible at the Rust level.** Existing callers that match on `WorkspacesEvent::TagApplied` need to change to match on `TagEvent::TagApplied`. Wire format unchanged.

## What must NOT change

- Wire-format event shapes — `{"type": "member_git_result", "ref": ..., "status": ...}` stays byte-identical.
- D9 event envelope.
- Method count or names — only return types change.
- DRY invariants from V5LIFECYCLE-11 / V5PARITY-12.

## Scope clarifications

- **All five services** participate: HyperforgeService, OrgsService, ReposService, WorkspacesService, BuildService, SecretsService.
- **Test-scoped methods** (`hyperforge.resolve_secret`, `repos.forge_port_schema`) follow the same convention.
- **Implementation pattern is the implementer's call** — macro vs hand-written, struct-wrapped variants vs flat. The contract is: each method's `T` enum has variants strictly equal to what the method emits, and the wire format is unchanged.

## Acceptance criteria

1. `cargo build` succeeds with every `#[plexus_macros::method]` returning a method-specific enum.
2. Existing wire-level integration tests (every `bash tests/v5/V5PARITY/V5PARITY-*.sh` and `tests/v5/V5LIFECYCLE/V5LIFECYCLE-*.sh`) pass without modification — proves wire compatibility.
3. `synapse -P $port -s lforge-v5 hyperforge workspaces` reports each method's distinct schema (not a single shared `WorkspacesEvent`).
4. A grep `grep -RE 'enum WorkspacesEvent|enum RepoEvent|enum BuildEvent' src/v5/` returns zero hits — the grab-bag enums are gone.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-17.sh` → exit 0 (tier 1; checks the schema-shape invariant via `synapse -s` introspection).
- Ready → Complete in-commit.
