---
id: ORGS-5
title: "`orgs_add` is idempotent on identical input; differences still fail"
status: Pending
type: implementation
blocked_by: []
unlocks: []
severity: Low
---

## Problem

ORGS-2 shipped `orgs_add` with *any* pre-existing org config producing a
failure event. That's too strict for the US-5 "scripted idempotent
bootstrap" user story, which is why US-5 was marked yellow on the
ORGS-4 checkpoint. Scripts have to guard every call with an
`orgs_list` + `jq` lookup just to re-run safely — a friction point
that's incidentally fine today and actively painful the first time any
declarative provisioning tool touches hyperforge.

The rule the user articulated: **if the existing on-disk config is
byte-for-byte identical to what `orgs_add` would write, the call is a
no-op success; if anything differs, it's still a failure**.

## Context

Today's behavior (from ORGS-2 implementation, commit `83d32d1`):

- Call `orgs_add --org foo --ssh.github /a`
- File `~/.config/hyperforge/orgs/foo.toml` appears with `ssh.github = "/a"`.
- Call `orgs_add --org foo --ssh.github /a` again.
- Failure event: `OrgAddFailed { reason: AlreadyExists }`.

Desired behavior after this ticket:

- Same setup.
- Second call with `--ssh.github /a` (identical input): success event
  identifying the outcome as a no-op ("already matches") and naming
  the org.
- Third call with `--ssh.github /b` (different value): failure event
  distinguishable from the no-op case and from every other failure —
  the caller can tell "you asked for a different state than what's
  already on disk".

The distinction is between *"this op is structurally equivalent to a
no-op"* and *"this op would overwrite a different state"*. The first
is idempotent success; the second is the existing AlreadyExists
failure, possibly with a richer reason that communicates the drift.

## Required behavior

### Capability: structural-equality detection

Before emitting an AlreadyExists failure, `orgs_add` compares the
would-be-written state to the on-disk state. If the two are
structurally identical — same ssh fields with same values, same
workspace_path — the call succeeds with an event identifying the
outcome as "already matches, no write performed".

Structural equality is defined on the persisted shape (the `OrgConfig`
values), not on serialized bytes. Whitespace, field ordering, or TOML
formatting differences that round-trip to the same `OrgConfig` are
irrelevant.

### Capability: observable distinction between the three outcomes

A caller inspecting only the event stream must be able to distinguish:

1. **Created** — no prior config existed; new file written.
2. **Already matches** — prior config existed with identical state; no
   write performed; not an error.
3. **Conflicts with existing** — prior config existed with different
   state; no write performed; is an error.

The discriminator is the implementer's choice (variant, reason enum,
message content) provided all three are unambiguous.

### Capability: idempotent scripts without guards

A shell loop that calls `orgs_add` with the same arguments twice in a
row no longer needs an `orgs_list` + `jq -e` guard to avoid a false
failure. The second call produces "already matches" instead of
"AlreadyExists".

### Capability: dry-run semantics unchanged

`dry_run: true` continues to produce a preview event without writing;
the dry-run path does not need new logic for the match-vs-conflict
distinction (callers using dry-run already opt into a "would do X"
framing).

## What must NOT change

- `orgs_update`, `orgs_list`, `orgs_delete`, `reload` behavior.
- `OrgConfig` struct shape.
- The existing AlreadyExists failure path — it continues to fire, but
  only when the existing state differs from the requested state.
- Validation order: invalid-name still short-circuits before any file
  inspection; the existence + structural-equality check is strictly
  after name validation and before the write.
- `orgs_add` tests from ORGS-2 that assert AlreadyExists on
  genuinely-differing input continue to pass.

## Acceptance criteria

1. Calling `orgs_add --org foo --ssh.github /a` on a fresh config dir
   persists a new org (unchanged from ORGS-2 behavior).
2. Calling `orgs_add --org foo --ssh.github /a` a second time with
   identical input succeeds; the on-disk file's mtime is unchanged
   (no rewrite); `orgs_list` still returns the same state.
3. Calling `orgs_add --org foo --ssh.github /b` after step 1 fails;
   the on-disk file is unchanged; the event stream identifies the
   failure as a state-conflict case distinguishable from both the
   fresh-create success and the already-matches success.
4. A verifier inspecting only the event stream can distinguish all
   three outcomes (created / already matches / conflicts) without
   reading service source.
5. Calling `orgs_add` with *no* `ssh` parameter and *no*
   `workspace_path`, twice in a row on the same org name, succeeds
   both times (the second as already-matches).
6. Mixed-field match: setting `{ssh.github: /a, workspace_path: /w}`
   then re-calling with the same two fields in either order succeeds
   as already-matches; calling with only `ssh.github: /a` (workspace
   path omitted) after the full setup is treated as a conflict (the
   caller's requested state omits a field the on-disk state has).
7. Dry-run against an already-matching state emits a preview event
   identifying the call as a no-op-equivalent dry-run; no file write.
8. US-5 in `plans/ORGS/artifacts/CHECKPOINT.md` is updated to 🟢 with a
   note pointing to this ticket as the closure. (This is the only
   CHECKPOINT.md change; other rows remain as they were.)
9. New integration tests in `tests/integration_test.rs` cover the
   three-outcome distinction, the mixed-field match, and the
   no-arguments idempotency case. `cargo test` passes.

## Completion

- `orgs_add` behavior updated per the capabilities above.
- Integration tests added.
- `README.md` cheatsheet row for `orgs_add` gains a one-line note about
  idempotent re-calls (something like "no-op if state matches; error
  if state differs").
- `~/.claude/skills/hyperforge/SKILL.md` cheatsheet gets the same note.
- CHECKPOINT.md US-5 row flipped to green.
- Status flipped to `Complete` in the same commit.
