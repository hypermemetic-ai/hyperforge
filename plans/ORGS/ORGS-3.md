---
id: ORGS-3
title: "`orgs_update` RPC for ssh map and workspace_path"
status: Complete
type: implementation
blocked_by: []
unlocks: [ORGS-4]
---

## Problem

Even once `orgs_add` (ORGS-2) exists, the only way to *change* an
existing org's SSH key map or workspace path is to hand-edit
`~/.config/hyperforge/orgs/<org>.toml`. This is particularly annoying
when rotating SSH keys or re-homing an org's workspace directory.

## Context

Same shared context as ORGS-2:

- `OrgConfig` at `src/config/org.rs:12-21`
- `OrgConfig::load(org)` and `OrgConfig::save(org)` are the existing
  I/O entry points.
- `orgs_delete` (`src/service.rs:967-1049`) demonstrates dry-run semantics
  and event stream style.

## Required behavior

### Capability: reuse the forge-key record from ORGS-2

The `ssh` parameter has the same capability contract as ORGS-2: a typed record with one optional field per known forge, driving per-forge CLI flags, rejecting unknown forges at parse time. This ticket does not re-define that record — it uses whatever ORGS-2 established.

### Capability: three distinct caller intents for `workspace_path`

The method must preserve three distinct caller intents. The distinction is observable in the persisted state after the call:

| Caller intent | How the caller expresses it | Observable result after the call |
|---|---|---|
| Keep current value | Omit the `workspace_path` flag entirely | The org's `workspace_path` (as returned by `orgs_list`) is whatever it was before the call |
| Clear the field | Pass `workspace_path` with an empty value | The org's `workspace_path` is absent / unset |
| Set to a specific value | Pass `workspace_path` with a non-empty value | The org's `workspace_path` equals the provided value |

The method signature must differentiate these three cases — an "empty string" must not collapse to "keep current." How that's achieved (e.g. by the natural shape of the host language's optional-string type) is an implementation detail.

### Capability: merge vs replace for ssh

The default behavior is per-field merge: a caller providing only `github` changes that field and leaves `codeberg` and `gitlab` untouched. A caller can opt into whole-record replacement via a `replace` flag, after which the persisted ssh state equals exactly what the caller provided (fields the caller did not set become unset).

### Capability: observable success and failure

Same principle as ORGS-2 — each case below is distinguishable from the others by inspecting only the event stream.

| Input | Outcome a caller can observe |
|---|---|
| Target `org` does not exist | Failure event; identifies the failure as "org not found" and names the org. No config is created. |
| All mutation fields omitted (neither `ssh` nor `workspace_path` given) | Failure event; identifies the failure as "no fields to update". No on-disk change. |
| `ssh` provided (default merge) | Success event; identifies the operation as a merge and names the org. On-disk ssh fields the caller provided are updated; fields the caller did not provide are unchanged. |
| `ssh` provided with replace mode | Success event; identifies the operation as a full replacement and names the org. On-disk ssh state equals exactly the caller's input. |
| `workspace_path` mutation (set or clear) | Success event; identifies whether the field was set or cleared and names the org. On-disk state follows the three-intents table above. |
| `dry_run: true` on any success path | Success event identifying the call as a preview. No on-disk change. |

"Identifies the failure / operation" is a capability constraint. Tests distinguish cases by whatever discriminator the implementer chooses, but a test MUST be able to distinguish them unambiguously.

### Capability: round-trip integrity

After any successful `orgs_update` call, `orgs_list` returns the org's state consistent with the caller's intents for `ssh`, `workspace_path`, and the merge / replace mode. An update call followed by a list call round-trips the caller's input losslessly.

## What must NOT change

- `orgs_add`, `orgs_list`, `orgs_delete`, `reload` behavior.
- `OrgConfig` struct fields.
- Existing test cases continue to pass.

## Acceptance criteria

1. Setup: after onboarding a test org `upd` with a github ssh value, `orgs_list` returns it with only the github field.
2. `orgs_update` on `upd` adding a codeberg value with `dry_run: true` leaves the persisted state unchanged; without `dry_run`, a subsequent `orgs_list` shows both github and codeberg present.
3. `orgs_update` on `upd` changing only the github value (default merge) updates github; codeberg remains as it was.
4. `orgs_update` on `upd` in replace mode with no ssh fields provided leaves the persisted ssh state empty (previous fields gone).
5. `orgs_update` on `upd` setting `workspace_path = "/tmp/x"` and then clearing it produces, after each call, the state described in the three-intents capability table — verified by reading back via `orgs_list`.
6. `orgs_update` with the `workspace_path` flag omitted entirely preserves the previous `workspace_path` value (verified after a prior set).
7. `orgs_update` on a non-existent org fails; the failure is distinguishable from a "no fields to update" failure by inspecting only the event stream.
8. `orgs_update` with no mutation fields fails with the "no fields to update" diagnosis.
9. `synapse … orgs_update --help` lists per-forge discoverable flags; no single flag accepts a JSON object for ssh.
10. For every acceptance criterion above that enumerates a success or failure, a test asserts the expected outcome by reading observable state (`orgs_list` output or the event stream) — no test reads service implementation source.
11. Round-trip integrity: after any sequence of successful `orgs_update` calls, `orgs_list` returns state consistent with the caller's intents across all three parameters (`ssh`, `workspace_path`, `replace` mode).
12. Integration test in `tests/integration_test.rs` covers every case above. `cargo test --test integration_test` passes.

## Completion

- Method added to `src/service.rs` under the root service activation.
- Integration tests for this method added.
- Documentation owned by this ticket is updated in the same commit:
  - `README.md` cheatsheet gains an `orgs_update` row with a one-line
    example.
  - `~/.claude/skills/hyperforge/SKILL.md` cheatsheet gains the same.
- All acceptance criteria pass from the command line on a clean
  workstation.
- Status flipped to `Complete` in the same commit.
