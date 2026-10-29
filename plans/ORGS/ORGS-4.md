---
id: ORGS-4
title: "Epic checkpoint — state of org CRUD against user stories"
status: Complete
type: checkpoint
blocked_by: [ORGS-2, ORGS-3]
unlocks: []
---

## Problem

ORGS-2 and ORGS-3 each ship with method-level integration tests
(success, error, dry-run paths for a single method). Those verify the
units; they don't verify that the units *compose* into the workflows
the epic actually exists to enable, and they don't produce the honest
signal a future reader needs to answer "did this epic deliver?".

This is the **epic checkpoint** ticket — following the pattern in
`~/dev/controlflow/hypermemetic/skills/skills/planning/SKILL.md` →
"Epic checkpoint — the state-of-the-epic ticket". Two deliverables:

1. **Automated verification of each user story** through the real
   Synapse → Plexus → service → filesystem round-trip.
2. **A state-of-the-epic map** — green/yellow/red per user story with
   a one-line note on anything non-green.

## Context

The agreed user stories are below, each with its exact Synapse command
sequence. The test harness runs each sequence in a temporary
`HYPERFORGE_CONFIG_DIR` so it cannot contaminate the developer's real
config. The pass criterion for each story is a specific observable
post-state plus (where relevant) the expected stream of
`HyperforgeEvent` variants.

### US-1 — Onboard a new org with SSH + workspace

```bash
synapse -P $PORT lforge hyperforge orgs_add \
    --org testorg \
    --ssh.github $TMPDIR/keys/gh \
    --workspace_path $TMPDIR/dev/testorg

synapse -P $PORT lforge hyperforge orgs_list
```

**Post-state:** `$HYPERFORGE_CONFIG_DIR/orgs/testorg.toml` exists,
contains `ssh.github = "$TMPDIR/keys/gh"` and `workspace_path =
"$TMPDIR/dev/testorg"`. `orgs_list` output includes `testorg` with
those fields.

### US-2 — Rotate an SSH key (merge semantics)

```bash
# Setup: org exists with github + codeberg keys
synapse -P $PORT lforge hyperforge orgs_add \
    --org testorg --ssh.github /a --ssh.codeberg /b

synapse -P $PORT lforge hyperforge orgs_update \
    --org testorg --ssh.github /c
```

**Post-state:** the on-disk TOML has `ssh.github = "/c"` AND
`ssh.codeberg = "/b"`. The codeberg key was not touched.

### US-3 — Re-home and then clear `workspace_path`

```bash
synapse -P $PORT lforge hyperforge orgs_update \
    --org testorg --workspace_path /new/path --dry_run true

synapse -P $PORT lforge hyperforge orgs_update \
    --org testorg --workspace_path /new/path

synapse -P $PORT lforge hyperforge orgs_update \
    --org testorg --workspace_path ""
```

**Post-state:** after the dry_run, the file is unchanged from its
pre-call state. After the second call, `workspace_path = "/new/path"`.
After the third, the field is absent from the serialized TOML
(serde's `skip_serializing_if = "Option::is_none"` applies).

### US-4 — Preview before writing

```bash
synapse -P $PORT lforge hyperforge orgs_add \
    --org typoed --ssh.github /x --dry_run true
```

**Post-state:** `$HYPERFORGE_CONFIG_DIR/orgs/typoed.toml` does NOT
exist. The event stream contains at least one `Info` variant whose
payload references the intended write (testable by substring match on
the org name).

### US-5 — Scripted idempotent bootstrap

```bash
for org in alpha beta; do
  if ! synapse -P $PORT lforge hyperforge orgs_list \
       | jq -e --arg o "$org" '.[$o]'; then
    synapse -P $PORT lforge hyperforge orgs_add \
        --org "$org" --ssh.github "/keys/$org"
  fi
done
# Re-run the entire loop
for org in alpha beta; do
  if ! synapse -P $PORT lforge hyperforge orgs_list \
       | jq -e --arg o "$org" '.[$o]'; then
    synapse -P $PORT lforge hyperforge orgs_add \
        --org "$org" --ssh.github "/keys/$org"
  fi
done
```

**Post-state:** both orgs exist after the first loop. The second loop
is a no-op (the `jq -e` short-circuit prevents duplicate-add errors).
No test assertion fails between the loops.

### US-6 — `--help` discoverability

```bash
synapse -P $PORT lforge hyperforge orgs_add --help
synapse -P $PORT lforge hyperforge orgs_update --help
```

**Post-state:** the output of each `--help` contains `--ssh.github`,
`--ssh.codeberg`, and `--ssh.gitlab` as separate flag listings (one
per line). Neither output contains a single `--ssh` flag that accepts
JSON. (Grep test: `grep '^\s*--ssh\s' <output>` returns no matches;
`grep '^\s*--ssh\.github' <output>` returns exactly one.)

### Anti-story — rename via delete+add composes

```bash
synapse -P $PORT lforge hyperforge orgs_add \
    --org oldname --ssh.github /a --workspace_path /w

# Capture shape
old=$(synapse -P $PORT lforge hyperforge orgs_list | jq '.oldname')

synapse -P $PORT lforge hyperforge orgs_add \
    --org newname \
    --ssh.github "$(jq -r '.ssh.github // empty' <<<"$old")" \
    --workspace_path "$(jq -r '.workspace_path // empty' <<<"$old")"

synapse -P $PORT lforge hyperforge orgs_delete \
    --org oldname --confirm true
```

**Post-state:** `newname.toml` exists with the original fields;
`oldname.toml` does not exist.

## Required behavior

### Part 1: automated verification

- The above scenarios run as automated test cases in a new
  `tests/e2e_user_stories.rs`.
- Each scenario runs in a temp `HYPERFORGE_CONFIG_DIR` that is cleaned
  up on drop, so test runs don't interfere with each other and don't
  touch the developer's real `~/.config/hyperforge`.
- Each scenario asserts its named post-state. A test fails if any
  assertion is off.
- `cargo test` (the default invocation) runs all seven scenarios.

### Part 2: state-of-the-epic map

A short markdown file at `plans/ORGS/artifacts/CHECKPOINT.md` (committed in the
same diff as the tests) containing:

- A table of user stories with green / yellow / red status.
- One sentence per yellow or red explaining what's missing and what
  would close it.
- A "deferred / discovered" section listing anything that came up
  during ORGS-2 or ORGS-3 that didn't fit in either (e.g. token
  lifecycle, `owner_type` field, rename RPC, bulk import).
- A one-paragraph re-pitch note: given what shipped, does org CRUD
  feel done, or is there a next epic to shape?

A plausible template:

```markdown
# ORGS epic checkpoint

## User stories

| ID | Story                                   | Status | Note |
|----|-----------------------------------------|--------|------|
| US-1 | Onboard new org                       | 🟢     | |
| US-2 | Rotate SSH key (merge)                | 🟢     | |
| US-3 | Re-home / clear workspace_path        | 🟢     | |
| US-4 | Preview before writing (dry_run)      | 🟢     | |
| US-5 | Scripted idempotent bootstrap         | 🟡     | Requires `jq`; no native idempotent flag yet |
| US-6 | `--help` discoverability              | 🟢     | |
| Anti | Rename via delete+add composition     | 🟢     | |

## Deferred / discovered
- Token lifecycle still in the auth sidecar — not a regression, but
  onboarding still requires two RPC hops.
- `owner_type: user|org` unused today; may be needed when a GitHub-org
  adapter issue surfaces.

## Re-pitch note
Org CRUD is done for the "one developer, few orgs" case. A follow-up
epic ORGS-BULK could address multi-org declarative import…
```

The tone is honest, not promotional. Yellow and red are valid end
states as long as they're named and explained.

## What must NOT change

- Existing method-level integration tests in `tests/integration_test.rs`
  remain.
- Runtime behavior of `orgs_add`, `orgs_update`, `orgs_list`,
  `orgs_delete`.

## Acceptance criteria

### Automated verification

1. `cargo test --test e2e_user_stories` runs all seven scenarios and
   all pass (or fail with honest red status reflected in the state
   map).
2. Each scenario's test name maps 1:1 to a story above
   (`test_us1_onboard`, `test_us2_rotate_ssh`, …, `test_us6_help`,
   `test_antistory_rename`).
3. Running the tests twice in a row passes both times (no state leaks
   between runs).
4. Running one scenario in isolation (`cargo test test_us2_rotate_ssh`)
   passes without needing the others to have run first.
5. When ORGS-2 or ORGS-3 ships a breaking change (e.g. the ssh param is
   renamed), at least one US test fails with a clear error message
   naming the broken contract — confirmed by intentionally introducing
   a break locally and observing the failure.
6. The test file includes a one-paragraph comment at the top
   referencing this ticket and explaining that the tests are the
   executable form of the epic's user stories, not method-level
   coverage.

### State-of-the-epic map

7. `plans/ORGS/artifacts/CHECKPOINT.md` exists, contains the user-story table
   with explicit status per story, and follows the template above.
8. Every yellow or red status row has a one-line explanation.
9. The re-pitch paragraph is present and commits to one of:
   *"done for now"*, *"next epic ORGS-X shapes the remaining problem"*,
   or *"epic goal was mis-sized, replan needed"*.

## Completion

- E2E test file is committed.
- `CHECKPOINT.md` is committed.
- All criteria pass.
- Status flipped to `Complete` in the same commit.
