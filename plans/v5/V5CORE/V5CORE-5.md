---
id: V5CORE-5
title: "hyperforge status method returns version and config_dir"
status: Complete
type: implementation
blocked_by: [V5CORE-2]
unlocks: [V5CORE-10]
---

## Problem

There is no wire-observable way to confirm the v5 daemon is the v5 daemon,
on what config directory, and at what version. Every later ticket's test
script will spawn the daemon and needs a cheap ping that also pins the
config directory being used.

## Required behavior

Add one method to `HyperforgeService` named `status`, no parameters. Its
success event is the **StatusEvent**, whose field set is pinned here:

| Field | Type | Notes |
|---|---|---|
| `version` | `String` | the crate version of the running daemon |
| `config_dir` | `FsPath` | the absolute, expanded config directory in use |

Wire invariants:

- Event type discriminator is the string `"status"` (`.type == "status"` in the NDJSON event stream).
- `version` is non-empty.
- `config_dir` is absolute and does not contain `..` or a trailing `/` (`FsPath` constraint from §types).

Edge cases:

- Daemon started with `--config-dir <path>`: `config_dir` reflects the expanded form of that path.
- No other method exists on `HyperforgeService` yet; `status` is additive, non-breaking.

## What must NOT change

- `HyperforgeService`'s lack of CRUD methods (invariant from README §3).
- Any pinned contract of V5CORE-2 (daemon port, registry name, zero children at this stage — the three stubs land in V5CORE-6/7/8 in parallel).

## Acceptance criteria

1. `hf_cmd status` against a freshly spawned daemon emits at least one event satisfying `.type == "status"`.
2. That event has a non-empty string at `.version`.
3. That event has a string at `.config_dir` that starts with `/` and contains neither `..` nor a trailing `/`.
4. When the daemon is started with `--config-dir <dir>`, `.config_dir` equals the absolute form of `<dir>`.
5. No other method is added to `HyperforgeService` by this ticket (schema introspection shows exactly one method on the root).

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-5.sh` → exit 0.
- Status flips in-commit with the implementation.
