---
id: V5PARITY-37
title: "AUTH-WALK — auth_requirements reports registry credentials; one key name per registry"
status: Pending
type: implementation
blocked_by: [V5PARITY-7]
unlocks: []
---

## Problem

`scripts/auth-walk.py` (2026-09-06) drives the owner to each provider's
token page, waits on the clipboard, validates, and stores through
`secrets set`. It asks the daemon which credentials it needs — but
`auth_requirements` only knows forge credentials (one row per org ×
`forge.credentials` entry). The registry tokens the build hub resolves
are invisible to it, so the script carries its own table of
`cargo/token`, `npm/token`, `pypi/token`, `hackage/token`, checked
against `secrets list_refs`. That table will drift.

The key names also disagree across layers: `src/auth/credentials.rs`
(v4 specs) names `crates-io/token` and a Hackage username + password;
`src/v5/build_hub.rs` resolves `cargo/token` / `npm/token` /
`pypi/token`; `src/package/hackage.rs` resolves `hackage/token`;
`src/auth_hub/types.rs` documents `cargo/token`. Only the build hub's
and the Hackage publisher's names are read by publish code.

## Required behavior

- `auth_requirements` emits, after the org rows, one `auth_requirement`
  per registry channel the build hub can publish through: `org:
  "build"`, `provider ∈ {crates.io, npm, pypi, hackage}`, `key:
  secrets://<the name publish code reads>`, `cred_type: token`,
  `present` from the store. Optional `channel` filter param.
- Each emitted row also carries `setup_url` and `scopes` (moved from
  the v4 `CredentialSpec` table into a v5 catalogue keyed by the same
  names), so a walker needs no table of its own. Forge rows carry them
  too.
- One name per registry, pinned in `docs/v5/methods.md`: `cargo/token`,
  `npm/token`, `pypi/token`, `hackage/token`. The v4 spec table's
  `crates-io/token`, `hackage/username`, `hackage/password` are deleted
  or aliased with a deprecation note; nothing reads them.
- `auth_check` probes registry tokens the way the walker does: an
  authenticated read (npm whoami, Hackage account page) or the
  nonexistent-target idiom (crates.io yank of a crate that does not
  exist: 404 authenticates, 403 "must be logged in" does not).
- `scripts/auth-walk.py` drops its `REGISTRY_KEYS` table and its
  per-provider URL/scope entries once the rows carry them; it keeps the
  clipboard loop and the validators until `auth_check` covers them.

## What must NOT change

- D9 secret redaction: `auth_requirements` never emits a resolved value.
- The forge rows' existing shape (V5PARITY-7's tests).

## Acceptance criteria

1. `auth_requirements` on this machine's config emits the four org rows
   plus four registry rows with `setup_url`; `present: true` for
   `cargo/token` after it was stored on 2026-09-06.
2. `auth_check` reports `valid: true` for `cargo/token` without
   publishing or yanking anything real.
3. `grep -rn 'crates-io/token\|hackage/username' src` is empty.
4. `scripts/auth-walk.py --dry-run` lists the same eight rows from the
   RPC alone, with no local table.

## Completion

Test output for the tier-1 script, the grep, and the status flip.
