# Hyperforge

Multi-forge repository management — declarative YAML config, typed RPC over WebSocket, ground-up rewrite as of v5.0.0.

Hyperforge syncs repositories across GitHub, Codeberg, and GitLab via direct REST APIs. It tracks your orgs, workspaces, and credentials in `~/.config/hyperforge/`, exposes ~80 RPC methods over a single daemon, and routes every git operation through a typed abstraction (subprocess for network ops, libgit2 for local).

## Install

```bash
cargo install --path .   # produces hyperforge, hyperforge-auth, hyperforge-ssh, hyperforge-legacy
```

## Quick start

If you've already authenticated with `gh`, the entire onboarding is two commands per org:

```bash
# Daemon
hyperforge --port 44104 --config-dir ~/.config/hyperforge &

# Onboarding (one RPC composes secret + org + credential + import)
synapse -P 44104 --json lforge-v5 hyperforge orgs bootstrap \
    --name <my-org> --provider github \
    --token gh-token:// --use_default_token true

# Materialize a checkout for everything tracked
synapse -P 44104 --json lforge-v5 hyperforge workspaces from_org \
    --org <my-org> --target_path ~/code/<my-org>

# Collect every token hyperforge still needs (forges + registries): opens
# each provider's token page, waits for the token on your clipboard,
# validates it, stores it via `secrets set`, clears the clipboard.
scripts/auth-walk.py            # --dry-run lists; --only cargo/token --force redoes one
```

That's the whole flow. See `docs/v5/getting-started.md` for the long version.

## Binaries

| Binary | Default port | Role |
|---|---|---|
| `hyperforge` | 44104 | Daemon: orgs, repos, workspaces, secrets, build (the canonical v5 surface) |
| `hyperforge-auth` | — | Secrets sidecar (YAML-backed secret store; v5 embeds it but the standalone binary is preserved) |
| `hyperforge-ssh` | — | SSH key management CLI (`V5PARITY-31` — currently only available on `hyperforge-legacy`; v5 covers the runtime via `repos.set_ssh_key`) |
| `hyperforge-legacy` | 44104 | The pre-5.0.0 daemon, preserved for one release cycle |

## Architecture at a glance

```
hyperforge (port 44104)
├─ HyperforgeHub (root)            ← status, begin, auth_*, config_*
│  ├─ OrgsHub      → orgs.*        ← list, create, bootstrap, set_credential
│  ├─ ReposHub     → repos.*       ← clone, fetch, pull, status, register, sync, …
│  ├─ WorkspacesHub → workspaces.* ← from_org, status, checkout, commit, tag, diff
│  ├─ SecretsHub   → secrets.*     ← set, list_refs, delete
│  └─ BuildHub     → build.*       ← unify, release, dist_init, run, exec
├─ ops::*                          ← typed wrappers (state, git, external_auth)
└─ adapters::*                     ← ForgePort: github, codeberg, gitlab
```

## CLI invocation

The daemon's namespace is `lforge-v5` (Plexus naming). Two equivalent forms:

```bash
# Standalone daemon (v5 default — recommended)
synapse -P 44104 --json lforge-v5 hyperforge <namespace> <method> --param value …

# When embedded in a substrate Plexus server
synapse substrate hyperforge <namespace> <method> --param value …
```

## Common operations

```bash
# Status across a workspace
synapse … workspaces status --name <ws>

# Pull every member
synapse … workspaces pull --name <ws>

# Create a coordinated tag across the whole workspace
synapse … workspaces tag --name <ws> --tag v0.5.0 --message "release"

# Adopt an existing local checkout into the registry
synapse … repos register --target_path ~/code/some-orphan

# Cut a release on a single repo (bump + tag + push + optional publish)
synapse … build release --org <org> --name <repo> --bump patch
```

## Config layout

```
~/.config/hyperforge/
├── config.yaml                    # provider_map, default_workspace
├── secrets.yaml                   # secrets://… resolved here (YAML-backed)
├── orgs/<org>.yaml                # one file per org: provider + credentials + repos
└── workspaces/<name>.yaml         # one file per workspace: name + path + members
```

Per-repo identity lives in `<repo>/.hyperforge/config.toml`. Distribution config lives in `<repo>/.hyperforge/dist.toml`.

## Migrating from v4

If you ran v4: see `MIGRATION.md`. Short version:
- `secrets.yaml` is file-compatible across versions.
- `orgs/<name>/repos.yaml` (v4 LocalForge) is **not** read by v5; v5 writes a single `orgs/<name>.yaml` with the repo registry inline.
- The `hyperforge` binary is now v5; the v4 daemon is preserved one release as `hyperforge-legacy`.

## Status

Production-ready for daily use. Twenty-five `V5PARITY-*` tickets shipped (see `plans/v5/V5PARITY/`); four v4-only features remain queued (release-asset upload, gitignore-sync, workspace check/verify, hyperforge-ssh CLI). Everything else from v4 is reachable.

## License

MIT
