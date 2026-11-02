# Migrating from hyperforge v4 to v5

> v4 is gone as of HYPERFORGE-52: `hyperforge-legacy`, `hyperforge-auth` and `hyperforge-ssh` no longer exist.

v5.0.0 (V5PARITY-32) makes v5 the canonical `hyperforge` binary. This doc explains what changes for an existing v4 user and how to do the handoff.

## TL;DR

```bash
# 1. Stop the v4 daemon (was on port 44104).
pkill -f "hyperforge --port 44104"

# 2. Move v4's per-org dirs aside (v5 reads a different format).
mv ~/.config/hyperforge/orgs/<each-org>/ ~/.config/hyperforge/orgs/<org>.v4.bak/

# 3. Start v5 (now the canonical hyperforge, default port 44104).
hyperforge --port 44104 --config-dir ~/.config/hyperforge &

# 4. Onboard each org with the new one-RPC flow.
synapse -P 44104 --json lforge-v5 hyperforge orgs bootstrap \
    --name <org> --provider github \
    --token gh-token:// --use_default_token true
```

## What's preserved across the upgrade

| Surface | Status |
|---|---|
| `secrets.yaml` (YAML-backed secret store) | **File-compatible.** v5 reads v4-written secrets without changes. |
| Per-repo `.hyperforge/config.toml` | **Same file, narrower schema.** v5 only knows `repo_name`, `org`, `forges`, `default_branch`, `visibility`, `description`. v4 keys (`ci`, `large_file_threshold_kb`, `dist`, `ssh`, `forge_config`) are rejected by v5's `deny_unknown_fields` parser. Strip them before v5 reads. |
| Workspace path conventions | Same — v5 honors absolute paths in `workspaces/<name>.yaml`. |
| `hyperforge-auth` sidecar | Still ships. v5 embeds the same secret store directly so the sidecar is optional under v5. |
| Default port (44104) | **Same.** v5 took the v4 port; you don't have to retrain muscle memory. |

## What changes

| Surface | v4 | v5 |
|---|---|---|
| Org config | `orgs/<name>/repos.yaml` (separate dir + LocalForge) | `orgs/<name>.yaml` (single file, provider + credentials + repos inline) |
| Workspaces | implicit (via `OrgConfig.workspace_path`) | explicit `workspaces/<name>.yaml`, first-class |
| Auth setup | `auth_setup` (interactive wizard) | `orgs.bootstrap --token gh-token://` (one RPC) |
| Token sharing | per-org duplicate entries | provider-default `secrets://github/_default/token` (V5PARITY-24) |
| Plexus namespace | `lforge` | `lforge-v5` (during the transition; will normalize in 6.0.0) |
| Activation tree | flat: `repo`, `workspace`, `build` | nested: `orgs`, `repos`, `workspaces`, `secrets`, `build` |

## v4 features still missing in v5 (Pending)

These are real workflow gaps. If you depend on any, stay on `hyperforge-legacy` until the corresponding ticket lands.

| v4 feature | v5 ticket | Status |
|---|---|---|
| `repos.assets` / `repos.upload` (release artifacts) | V5PARITY-28 | Pending |
| `build.gitignore_sync` | V5PARITY-29 | Pending |
| `workspace.check` / `workspace.verify` | V5PARITY-30 | Pending |
| `hyperforge-ssh` standalone CLI | V5PARITY-31 | Pending — runtime is in `repos.set_ssh_key` already |

Track progress in `plans/v5/V5PARITY/`.

## Running v4 alongside v5

The v4 binary builds as `hyperforge-legacy` for one release cycle. Run it on a different port if you need both:

```bash
hyperforge-legacy --port 44103 &   # v4 on a free port
hyperforge --port 44104 &          # v5 on the canonical port
```

`hyperforge-legacy` will be **removed in 6.0.0**. Migrate any automation away from it.

## Verifying the handoff

```bash
# 1. v5 daemon up?
synapse -P 44104 --json lforge-v5 hyperforge status

# 2. Orgs registered?
synapse -P 44104 --json lforge-v5 hyperforge orgs list

# 3. Auth works against the forge?
synapse -P 44104 --json lforge-v5 hyperforge auth_check --org <org>

# 4. Workspace state matches disk?
synapse -P 44104 --json lforge-v5 hyperforge workspaces status --name <ws>
```

If all four return without errors, v5 is fully your daily driver.
