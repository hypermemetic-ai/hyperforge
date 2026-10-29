---
id: V5CORE-10
title: "CORE checkpoint — scaffolding composes end-to-end"
status: Complete
type: checkpoint
blocked_by: [V5CORE-2, V5CORE-3, V5CORE-4, V5CORE-5, V5CORE-6, V5CORE-7, V5CORE-8, V5CORE-9]
unlocks: []
---

## Problem

Each V5CORE implementation ticket is verified in isolation. We need a
single green checkpoint that proves the composed surface satisfies the
CORE user stories — no new behavior, just aggregation.

## User stories → verifying scripts

| # | User story | Verified by |
|---|---|---|
| U1 | "The v5 daemon starts cleanly on port 44105 without touching v4." | `tests/v5/V5CORE/V5CORE-2.sh` |
| U2 | "`hyperforge status` returns version and config_dir via synapse." | `tests/v5/V5CORE/V5CORE-5.sh` |
| U3 | "The three child services (orgs, repos, workspaces) are discoverable in schema with zero methods each." | `tests/v5/V5CORE/V5CORE-6.sh`, `V5CORE-7.sh`, `V5CORE-8.sh` |
| U4 | "Every config fixture round-trips losslessly." | `tests/v5/V5CORE/V5CORE-3.sh` |
| U5 | "A `secrets://` reference resolves through the embedded secret store." | `tests/v5/V5CORE/V5CORE-4.sh` |
| U6 | "The shared test harness spawns, runs, and tears down a daemon cleanly." | `tests/v5/V5CORE/V5CORE-9.sh` |

## State-of-epic map

Filled at checkpoint evaluation time. Target terminal state: all green.

| Story | Status | One-liner |
|---|---|---|
| U1 daemon starts | green | `hyperforge-v5` binds 44105, registers as `lforge-v5`, v4 on 44104 unaffected |
| U2 status | green | `hf_cmd status` emits `{type:"status", version, config_dir}` |
| U3 three stubs | green | `orgs`, `repos`, `workspaces` static children present, zero wire methods each |
| U4 round-trip | green | `empty/` and `minimal_org/` fixtures load and round-trip |
| U5 secret resolve | green | `secrets://<path>` resolves through the embedded YAML store |
| U6 harness | green | `hf_spawn`/`hf_cmd`/`hf_teardown` cycle cleanly, tiers honoured |

## Required behavior

The checkpoint introduces **zero new daemon behavior**. Its test script
runs every sibling V5CORE-*.sh in turn, aggregates pass/fail, and prints
the state-of-epic map. It does not spawn additional synapse commands
beyond what those scripts spawn.

Edge cases:

- Any sibling script failing → checkpoint fails; the map marks that row red; other rows still report their own status.
- A sibling script missing → checkpoint fails with a diagnostic naming the missing file.

## What must NOT change

- This ticket must not add methods, activations, fixtures, or helpers.
- If a user story cannot be verified with the sibling scripts as written, the fix is to the **sibling ticket** (amend its acceptance criteria), not this one.

## Acceptance criteria

1. Every sibling script `V5CORE-{2,3,4,5,6,7,8,9}.sh` is present and executable.
2. `bash tests/v5/V5CORE/V5CORE-10.sh` runs each sibling script and exits 0 iff every sibling exited 0.
3. The checkpoint script prints one line per user story in the form `U<N> <green|yellow|red>: <one-liner>` in the order U1..U6.
4. The checkpoint script introduces no new synapse RPC calls and no new fixtures beyond those already present.

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-10.sh` → exit 0.
- Status flips in-commit with the last implementation ticket it aggregates.
