---
id: TRANSPORT-2
title: "Caller-controlled remote-URL transport for a hyperforge-registered repo"
status: Complete
type: implementation
blocked_by: []
unlocks: [TRANSPORT-3]
severity: Medium
---

## Problem

`repo init` today unconditionally rewrites a repo's remote(s) to SSH
form (`git@<forge>:<org>/<repo>.git`). The caller cannot express a
preference. After init, there is no hyperforge-native way to flip the
transport — only bypassing hyperforge with `git remote set-url` works.

This makes hyperforge fragile in two scenarios:

1. **Init on a machine without SSH infrastructure** (no keys, no
   agent). Init succeeds; every subsequent `repo push` / `repo sync`
   fails silently-until-used. The user has to read the init warning
   carefully, which many callers (especially scripts) won't.
2. **Transport drift recovery.** If SSH breaks (key rotated and not
   replaced; org moved to a new host; SSH disabled for a forge), the
   user has no hyperforge command to switch to HTTPS. They must leave
   the tool.

## Required behavior

### Capability: transport preference at init time

`repo init` accepts a transport preference. Valid values match the
transports the forge supports (today: `ssh`, `https`). Omitting the
preference preserves today's default (`ssh`) so existing callers are
unaffected.

| Init call | Resulting remote for `github` |
|---|---|
| transport preference omitted | SSH form (unchanged from today) |
| transport = `ssh` | SSH form (same) |
| transport = `https` | HTTPS form (new) |
| transport = anything else | Failure event; no write |

### Capability: transport change after init

A hyperforge method exists for switching the transport of an
already-initialized repo's remote(s), idempotently. Given the current
on-disk state and the caller's requested transport:

| Current transport | Requested transport | Effect |
|---|---|---|
| Already matches requested | any | No change to `git remote`; success event identifying the call as a no-op |
| Differs from requested | any valid transport | Remote URL rewritten to the requested form; success event identifying the change |
| Requested transport is invalid or unsupported by the forge | any | Failure event; no change |
| Target repo is not hyperforge-registered | any | Failure event (same class as other pre-init methods — caller runs `repo init` first) |

Idempotency is a hard requirement: running the same call twice in a
row, with the same inputs, is a no-op the second time and does not
emit a failure. This matches the pattern set by ORGS-5 for
`orgs_add`.

### Capability: observable current transport

The transport of a hyperforge-registered repo is readable without
parsing `git remote -v` externally. Either `repo status` gains a
transport field, or a dedicated read method exists. Either way, a
caller can determine the current transport without leaving
hyperforge.

### Capability: HTTPS transport uses forge-native URL shape

HTTPS transport means `https://<forge-host>/<org>/<repo>.git`
(github-style), not any alternative auth-in-URL pattern. Tokens and
credentials remain the caller's responsibility (via `gh`'s
credential-helper for GitHub, or equivalent); hyperforge does not
embed credentials in the remote URL under any circumstance.

### Capability: no silent regression for existing callers

Callers of `repo init` that do not pass a transport preference
continue to get SSH, as today. The only behavioral change on that
path is that a new optional flag is discoverable in `--help`.

`repo push`, `repo sync`, `repo status` all continue to work on
repos whose transport was set before this ticket — nothing on disk
is re-migrated.

## What must NOT change

- `orgs_*` methods.
- `repo init`'s other behaviors (LocalForge registration, `.hyperforge/config.toml`
  creation, pre-push hook installation, SSH wrapper config).
- `OrgConfig` or per-repo `config.toml` schema, except insofar as a
  new optional field representing the current transport may be
  required to make `repo status` observable.
- `gh`-based HTTPS credential flow. This epic does not touch credential
  helpers; it only changes which URL form is stored in the remote.

## Acceptance criteria

1. `repo init --path P --org O --forges github --transport ssh` produces a remote at `git@github.com:O/N.git` for the registered repo `N`.
2. `repo init --path P --org O --forges github --transport https` produces a remote at `https://github.com/O/N.git`.
3. `repo init --path P --org O --forges github` (no transport flag) produces an SSH remote — preserving today's default.
4. On a repo already initialized as SSH, a transport-switch call with target `https` rewrites the `origin` remote to the HTTPS form. A subsequent `git remote -v` shows only the HTTPS URL.
5. Step 4 run a second time with the same target (`https`) succeeds as a no-op — the event stream identifies it as already-matching, no `git remote` invocation happens, and no error is emitted. (Idempotency mirrors the rule set by ORGS-5.)
6. Transport-switch on a non-initialized repo path fails with a distinguishable event; no `git remote` invocation happens.
7. Transport-switch with an unsupported transport value fails at the CLI parse layer (if the transport arg is a closed-set type) or with a distinguishable failure event (if open-string), and no on-disk change occurs.
8. `repo status` (or an equivalent read method) reports the current transport for a hyperforge-registered repo. Switching transport via the new method updates the reported value on the next `repo status` call.
9. For any repo that was previously initialized under today's behavior (pre-TRANSPORT), `repo status` correctly reports its current transport (read from `git remote`, not from a cached field), and `repo push` / `repo sync` still work unchanged.
10. New integration tests cover: each init-with-transport path; idempotent switch; switch from ssh to https and back; pre-existing-repo compatibility; failure paths (not-initialized, invalid transport). `cargo test` passes.

## Completion

- New method(s) added to the `repo` subservice; `repo init` gains a
  transport flag.
- Integration tests added.
- README and `~/.claude/skills/hyperforge/SKILL.md` cheatsheets document
  the new flag and method in the same commit.
- Status flipped to `Complete`.
