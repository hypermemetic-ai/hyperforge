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
│  ├─ BuildHub     → build.*       ← unify, release, dist_init, run, exec
│  └─ CiHub        → ci.*          ← add, remove, pipelines
│     └─ PipelineHub → ci.<name>.* ← describe, start, trigger, runs, get, watch, log, cancel
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

## Pipelines (`ci`)

A repo declares pipelines in `.hyperforge/pipelines.toml`; each registered
pipeline is a dynamic child of `ci` (HYPERFORGE-22). The file is read at
every call, so an edit is live without a restart.

```toml
[[pipeline]]
name = "menger-app"
env = { IMAGE_STORE = "local" }     # pipeline-wide; a stage's env overrides it

[[pipeline.stage]]
name = "build"
run = "deploy/pipeline image"      # sh -c, in the repo's directory
trigger = "auto"                   # runs when the run starts
outputs = ["image", "release"]     # key=value lines the stage writes to $HF_OUTPUTS

[[pipeline.stage]]
name = "deploy-local"
needs = ["build"]                  # an earlier stage; its outputs arrive as $IMAGE and $HF_BUILD_IMAGE
trigger = "manual"                 # the default
run = "deploy/pipeline local --image \"$IMAGE\""
timeout_minutes = 30
```

```bash
synapse -P 44104 lforge-v5 hyperforge ci add --path ~/code/menger      # register a repo's pipelines
synapse -P 44104 lforge-v5 hyperforge ci pipelines                     # every pipeline, its repo, its stages
synapse -P 44104 lforge-v5 hyperforge ci menger-app describe
synapse -P 44104 lforge-v5 hyperforge ci menger-app start              # a new run; its auto stages run
synapse -P 44104 lforge-v5 hyperforge ci menger-app trigger --run 7 --stage deploy-local
synapse -P 44104 lforge-v5 hyperforge ci menger-app runs --limit 20
synapse -P 44104 lforge-v5 hyperforge ci menger-app get --run 7
synapse -P 44104 lforge-v5 hyperforge ci menger-app watch --run 7      # --follow true stays open
synapse -P 44104 lforge-v5 hyperforge ci menger-app log --run 7 --stage build
synapse -P 44104 lforge-v5 hyperforge ci menger-app cancel --run 7 --stage deploy-local
synapse -P 44104 lforge-v5 hyperforge ci remove --path ~/code/menger
```

A stage's environment holds the pipeline's and the stage's `env`,
`HF_PIPELINE`, `HF_RUN`, `HF_STAGE`, `HF_ATTEMPT`, `HF_OUTPUTS` (the file to
write outputs to) and each needed stage's outputs. Exit 0 with every declared
output written is `succeeded`; a non-zero exit, a missing output or a timeout
is `failed`; `cancel` is `canceled` and stops the stage's whole process group.
Triggering a stage again is a new attempt. Where an artifact lives is the
stage's business (its `env`), never hyperforge's.

The registered repos are kept in `~/.config/hyperforge/ci.yaml`; runs,
attempts, outputs and logs in `~/.local/state/hyperforge/ci/` (`--state-dir`).
A stage running when hyperforge stops is `failed` ("hyperforge restarted")
when it comes back.

## Run as a service

`deploy/systemd/hyperforge.service` runs `~/.local/bin/hyperforge-v5` on
44104 under `systemd --user`, with the same config and state dirs as a
manual run:

```bash
cargo build --release --bin hyperforge-v5
install -m755 "${CARGO_TARGET_DIR:-target}/release/hyperforge-v5" ~/.local/bin/hyperforge-v5
install -Dm644 deploy/systemd/hyperforge.service ~/.config/systemd/user/hyperforge.service
systemctl --user daemon-reload
systemctl --user enable --now hyperforge.service
synapse -P 44104 lforge-v5 hyperforge status                          # answers
journalctl --user -u hyperforge -f                                    # its log
```

After installing a new binary: `systemctl --user restart hyperforge`.

## Config layout

```
~/.config/hyperforge/
├── config.yaml                    # provider_map, default_workspace
├── secrets.yaml                   # secrets://… resolved here (YAML-backed)
├── orgs/<org>.yaml                # one file per org: provider + credentials + repos
├── workspaces/<name>.yaml         # one file per workspace: name + path + members
└── ci.yaml                        # repos whose .hyperforge/pipelines.toml is registered
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
