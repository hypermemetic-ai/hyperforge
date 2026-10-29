---
id: V5PARITY-7
title: "AUTH — secrets.{set, list_refs, delete} + auth_check + auth_requirements"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-12]
---

## Problem

Today the only way to get a token into v5 is hand-edit `secrets.yaml` or use the test-harness `hf_put_secret` helper. There's no RPC method; no way to probe whether configured credentials actually work; no way to list what secrets exist.

## Required behavior

**New activation: `SecretsService`** — static child on `HyperforgeService`, alongside `orgs/repos/workspaces`.

| Method | Inputs | Behavior |
|---|---|---|
| `secrets.set --key K --value V [--dry_run]` | `SecretRef` (`secrets://<path>`), `String` | writes to `secrets.yaml` via `ops::secrets::put_secret` (new wrapper). Masks the value in the emitted event. |
| `secrets.list_refs` | — | streams the `SecretRef` set (keys only — never values) as `secret_ref { key, type_hint }` events |
| `secrets.delete --key K [--dry_run]` | `SecretRef` | removes the entry from `secrets.yaml` |

**New RPC methods on HyperforgeService (root):**

| Method | Behavior |
|---|---|
| `auth_check [--org X]` | For each org (or the named one), iterate its `CredentialEntry` list; resolve each secret; for tokens, probe the forge's `repo_exists` against a known-public repo (e.g. `<org>/.github` on GitHub) to test the credential; emit per-cred `auth_check_result { org, key, valid: bool, message? }` + aggregate |
| `auth_requirements [--org X]` | Read-only: report what credential refs exist + what forges need them based on the org's remotes |

## What must NOT change

- `secrets.yaml` stays the only backing store for v5. (Pluggable backends are a post-V5PARITY concern — V5AUTH-PLUGGABLE or similar.)
- No value ever appears in an event payload — not on set (the write event masks), not on list (keys only), not on auth_check (boolean validity + optional message).
- D9 envelope + secret-redaction rule (CONTRACTS §types) preserved.

## Acceptance criteria

1. `secrets.set --key "secrets://github/test/token" --value XYZ` writes to `secrets.yaml`. The emitted event masks the value (e.g. `value_length: 3` instead of `"XYZ"`).
2. `secrets.list_refs` returns the key that was set; does NOT return the value.
3. `secrets.delete --key "secrets://github/test/token"` removes the entry; subsequent `list_refs` shows no such key.
4. `auth_check --org X` with a valid token emits `{ valid: true }` per cred; with a blank token emits `{ valid: false, message: "auth" }`.
5. `auth_requirements --org X` for an org with remotes on github + codeberg reports both forges as needing credentials.
6. No token value appears in any grep-able event stream — V5LIFECYCLE-11's grep extends to cover this: `grep '<known-test-token>'` on any event stream returns empty.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-7.sh` → exit 0.
- Ready → Complete in-commit.
