---
id: TRANSPORT-3
title: "Epic checkpoint — transport control end-to-end"
status: Complete
type: checkpoint
blocked_by: [TRANSPORT-2]
unlocks: []
---

## Problem

TRANSPORT-2 ships method-level integration tests for each behavior in
isolation. This checkpoint verifies that the behaviors compose into
the workflows the epic was motivated by — specifically the recovery
workflow, which is the story that forced the epic into existence.

## Deliverables

### Part 1: automated verification

A new `tests/e2e_transport.rs` file (or a dedicated section of an
existing E2E file) with these scenarios, each asserting its named
post-state through the shipped service surface:

- **TS-1 — init with SSH (default).** Init a fresh repo without a
  transport flag; remote ends up SSH; transport readable via
  `repo status`.
- **TS-2 — init with HTTPS.** Same, but with `--transport https`;
  remote ends up HTTPS.
- **TS-3 — switch SSH → HTTPS after init.** Init SSH, then switch to
  HTTPS. Remote is HTTPS; `repo status` reflects the change.
- **TS-4 — switch HTTPS → SSH.** Inverse of TS-3.
- **TS-5 — idempotent no-op.** Two successive switches to the same
  transport. Second call emits no-op-equivalent event; no git command
  is invoked for the redundant switch.
- **TS-6 — recovery workflow.** Simulate the original incident:
  init-with-SSH on a repo whose remote was HTTPS and whose machine
  has no SSH keys. Verify the switch-to-HTTPS call repairs it
  (remote back to HTTPS, subsequent `repo status` clean, no SSH
  operations attempted).
- **TS-7 — pre-existing-repo compatibility.** A repo initialized
  *before* this epic (simulated by manually creating a `.hyperforge`
  config without the transport field) reports its transport correctly
  via `repo status` and can be switched without needing re-init.

### Part 2: state-of-the-epic map

A short markdown file at `plans/TRANSPORT/artifacts/CHECKPOINT.md`
containing the usual green/yellow/red table per story, a
deferred/discovered section, and a one-paragraph re-pitch note.

The "deferred / discovered" section should note that codeberg / gitlab
transport switching was deliberately deferred (listed in the epic's
Out of scope) and that this stays true until a real codeberg/gitlab
workflow surfaces.

## What must NOT change

Same as TRANSPORT-2. No code changes from the checkpoint — this
ticket only verifies.

## Acceptance criteria

1. `cargo test --test e2e_transport` runs all seven TS-* scenarios and they all pass.
2. Each scenario's test name maps 1:1 to its ID (`test_ts1_init_ssh`, `test_ts2_init_https`, …, `test_ts7_pre_existing_repo`).
3. Running the tests twice in a row succeeds both times (no state leaks).
4. Running one scenario in isolation passes without requiring the others to have run first.
5. `plans/TRANSPORT/artifacts/CHECKPOINT.md` exists with the epic's user stories scored and an honest re-pitch note that commits to one of: *"done for now"*, *"next epic shapes …"*, or *"goal was mis-sized, replan needed"*.
6. Every yellow or red row in CHECKPOINT.md has a one-line explanation.

## Completion

- E2E tests committed.
- CHECKPOINT.md committed in the same diff.
- Status flipped to `Complete`.
