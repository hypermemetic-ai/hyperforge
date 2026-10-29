---
id: V5PARITY-8
title: "CLI — reload, config_show, config_set_ssh_key, config_show_ssh_key, begin"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-12]
---

## Problem

Small-but-missing root-level ergonomics from v4. None on their own is a day's work; together they're the polish that makes v5 self-explanatory.

## Required behavior

Five new methods on `HyperforgeService` (root):

| Method | Behavior |
|---|---|
| `reload` | Discards any in-memory config cache + re-reads all yaml from disk. Emits `reload_done { orgs: N, workspaces: M, secrets_refs: K }`. v5 currently re-reads per-call so this is essentially a no-op, BUT if V5PARITY-12's cleanup introduces a memoized cache (typed LocalForge equivalent), `reload` is the invalidator. |
| `config_show` | Returns the global `config.yaml` contents as `{ default_workspace?, provider_map: {domain: provider} }`. |
| `config_set_ssh_key --org X --forge F --key P` | Adds/updates a `CredentialEntry { key: FsPath(P), type: ssh_key }` on the given org's forge block. Wrapper over `orgs.set_credential` with SSH-key-specific ergonomics. |
| `config_show_ssh_key --org X [--forge F]` | Returns the SSH key path(s) configured for an org (or filtered by forge). Never reveals file CONTENTS — only paths. |
| `begin` | Guided onboarding: checks whether `$HOME/.config/hyperforge/` exists, creates it if not, initializes an empty `config.yaml`, and emits a `next_steps` event listing concrete commands (`orgs.create`, `secrets.set`, etc.) the user likely wants next. Idempotent on pre-existing configs. |

All five follow D9 event envelope.

## What must NOT change

- `orgs.set_credential` still the canonical way to add any credential type. `config_set_ssh_key` is an alias with an opinionated shape, not a replacement.
- No method in this ticket touches external forges.

## Acceptance criteria

1. `reload` on a pre-spawned daemon succeeds and emits `reload_done` with the current counts.
2. `config_show` returns the `provider_map` as configured.
3. `config_set_ssh_key --org X --forge github --key ~/.ssh/id_ghx` updates the org yaml; `orgs.get --name X` shows a `ssh_key`-typed credential with the expanded path.
4. `config_show_ssh_key --org X --forge github` returns the path; `--forge codeberg` when not configured returns a `not_configured` event, not an error.
5. `begin` on an empty `$HF_CONFIG` creates `config.yaml` with a default `provider_map` (the three known forges) and emits the onboarding hint events. Re-run is a no-op.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-8.sh` → exit 0.
- Ready → Complete in-commit.
