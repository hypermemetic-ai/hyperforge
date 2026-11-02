# Doctor: repos that moved

When a repo is transferred or renamed on the forge, the registry and your checkout keep the old owner. The doctor asks the forge who owns each repo and compares.

```bash
hf repos doctor --org hypermemetic
```

Read-only. One `repo_doctor_entry` per repo, then a summary:

- `clean`: the forge agrees (owner case and `owner_aliases` are not a difference).
- `diverged`: the forge says another owner; `canonical_owner` names it.
- `unknown`: nothing was asked or the forge didn't answer; `reason` says which (for example, the repo's `forges` scope excludes every remote).

## Heal

Heal is a preview until you say otherwise:

```bash
hf repos doctor --org hypermemetic --name jsexec --heal true --path ~/dev/controlflow/hypermemetic/jsexec
hf repos doctor --org hypermemetic --name jsexec --heal true --path ~/dev/controlflow/hypermemetic/jsexec --dry-run false
```

A heal moves the registry entry to the canonical owner (or drops the stale one when that owner already has it, `already_in_target`), rewrites workspace refs, flips the checkout's `.hyperforge/config.toml` org when it has one, and points the checkout's remotes on that forge at the new owner. Remotes on other forges are left alone. `--path` names the checkout; without it only the registry moves. Each run writes a rename map to the config dir.

`repos sync --heal true` does the same during a sync.

## Whole orgs

`hf workspaces org_diff` reports what moved between two orgs; `hf repos migrate_org` and `hf workspaces migrate_org` move repos you choose. Both preview by default; `--dry-run false` writes.

## Aliases

If two owner names are the same identity (an org and the user that owns it), list them in `config.yaml` so they are never a divergence:

```yaml
owner_aliases:
  hypermemetic-ai:
    - hypermemetic-legacy-user
```
