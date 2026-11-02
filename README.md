# Hyperforge

Multi-forge repository management — declarative YAML config, typed RPC over WebSocket, ground-up rewrite as of v5.0.0.

Hyperforge syncs repositories across GitHub, Codeberg, and GitLab via direct REST APIs. It tracks your orgs, workspaces, and credentials in `~/.config/hyperforge/`, exposes ~80 RPC methods over a single daemon, and routes every git operation through a typed abstraction (subprocess for network ops, libgit2 for local).

## Install

```bash
cargo install --path .   # produces hyperforge and hyperforge-v5
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

## Architecture at a glance

```
hyperforge (port 44104)
├─ HyperforgeService (root)            ← status, begin, auth_*, config_*
│  ├─ OrgsService      → orgs.*        ← list, create, bootstrap, set_credential
│  ├─ ReposService     → repos.*       ← clone, fetch, pull, status, register, sync, …
│  ├─ WorkspacesService → workspaces.* ← from_org, status, checkout, commit, tag, diff
│  ├─ SecretsService   → secrets.*     ← set, list_refs, delete
│  ├─ BuildService     → build.*       ← unify, release, dist_init, run, exec
│  └─ CiService        → ci.*          ← add, remove, pipelines
│     └─ PipelineService → ci.<name>.* ← describe, start, trigger, runs, get, watch, log, cancel
├─ ops::*                          ← typed wrappers (state, git, external_auth)
└─ adapters::*                     ← ForgePort: github, codeberg, gitlab
```

### Naming: services (2026-10-29)

A Plexus backend is a *service* (owner, 2026-10-29). Every activation type
was renamed from its old suffix to `Service` — `HyperforgeService`,
`OrgsService`, `ReposService`, `WorkspacesService`, `SecretsService`,
`BuildService`, `CiService`, `PipelineService` (v5), `RepoService`,
`WorkspaceService`, `ImagesService`, `ReleasesService`, `AuthService`
(legacy). **Call paths are unchanged**: every namespace and method name
(`hyperforge repos …`, `build …`, `ci …`, `secrets …`) and every event
field stayed as it was. What a client can see change is the schema's
`activation` field (the type name: `"HyperforgeService"` where it said the
old name), so a client generated from the schema names its types anew on
regeneration. The registry shim asks the registry service's root as
`registry-service`, falling back to plexus-registry's current default
until that repository is renamed. `scripts/naming-gate.sh` (run by
`cargo test --test naming_gate`) fails on any remnant of the old word.

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

## Releasing a crate chain (`build`)

A chain of crates spread over several repos — members of cargo workspaces
too — is released from a workspace (HYPERFORGE-23). `--include` names crates
(globs on crate names), never repos:

```bash
W="synapse -P 44104 lforge-v5 hyperforge build"
$W package_diff --name hypermemetic --include 'plexus-*'        # each crate against crates.io, in dependency order
$W bump --name hypermemetic --include plexus-core --include plexus-transport --bump patch --dry_run true
$W bump --name hypermemetic --include plexus-core --include plexus-transport --bump patch
$W publish --name hypermemetic --include menger                 # dry run of the whole chain menger needs
$W publish --name hypermemetic --include menger --execute true  # for real
```

- `package_diff` says each crate's `status`: `unpublished`, `ahead` (a
  version crates.io lacks), `published` (its source unchanged since),
  `drifted` (published, but the files differ from the published `.crate` —
  bump it), `behind`, `yanked`.
- `bump` sets each included crate's version and moves every dependent's
  `version =` requirement on it that matched the old version: inside the
  bumped set to the new version (`0.6` → `0.6.1`, the operator kept);
  elsewhere only a path dependency, and only when it would no longer match
  (a patch leaves `0.6` alone; a registry pin on an older line, `0.5`, is
  never touched). It refuses to write over an
  uncommitted manifest, refreshes each repo's `Cargo.lock`, and commits each
  repo once (`--commit false` leaves the edits).
- `publish` takes what the included crates need first (`--closure false`
  takes only them), skips a version crates.io holds, and groups the rest in
  dependency order — one `cargo publish -p … -p …` per cargo workspace. It
  dry-runs **every** group first, each against the others' checkouts (a
  `[patch.crates-io]` cargo config), and publishes nothing unless all pass;
  `--execute true` then publishes group by group, stops at the first failure
  naming what is and is not published, and tags each crate
  `<crate>-v<version>` (local tags). An execute refuses uncommitted changes.
  The token is `secrets://cargo/token` (else cargo's own login), handed to
  cargo in its environment, never on a command line. Logs:
  `~/.local/state/hyperforge/publish/<time>/`.
- A repo whose crates need their own publish step declares it in
  `.hyperforge/config.toml`; hyperforge runs it (`sh -c`, in the repo) in
  place of `cargo publish` with `HF_PUBLISH_MODE` (`dry-run` / `execute`),
  `HF_PUBLISH_CRATES`, `HF_CARGO_CONFIG` (the patch, for a dry run — pass it
  to cargo as `--config "$HF_CARGO_CONFIG"`) and, on execute,
  `CARGO_REGISTRY_TOKEN`:

```toml
[publish]
crates = ["plexus-shell", "menger-tickets-api", "plexus-login", "menger"]
dry_run = "scripts/publish.sh dry-run"
execute = "scripts/publish.sh publish"
```

### Cabal packages and Hackage

A repo's root `.cabal` package is in the chain beside the crates
(HYPERFORGE-26): `package_diff` and `publish` take it to Hackage by the rules
above, ordered by `build-depends`, each package a group of its own. A glob
like `plexus-*` catches both kinds, so read the plan before `--execute`.

```bash
$W publish --name hypermemetic --include synapse                 # dry run: plexus-protocol, then plexus-synapse
$W publish --name hypermemetic --include synapse --execute true  # for real
synapse -P 44104 lforge-v5 hyperforge build publish --org hypermemetic --name plexus-protocol   # one repo: its dry run
```

- `--include` / `--exclude` match the package name or, for a Cabal package,
  its repo's name: `synapse` and `plexus-synapse` both select `plexus-synapse`.
  The closure follows `build-depends`.
- The dry run is `cabal check` (warnings are reported, not failures; an error
  fails) and `cabal sdist` into the run's log dir. `--execute true` uploads
  that tarball with `cabal upload --publish`, skips a version Hackage holds
  (deprecated ones too), refuses uncommitted changes, stops at the first
  failure and tags `<package>-v<version>`, as for cargo.
- One repo: `--channel hackage`, or no channel on a repo with a `.cabal` file
  and no Cargo.toml.
- The token is `HACKAGE_TOKEN`, else `secrets://hackage/token`, else the
  `password-command` in `~/.cabal/config`, else cabal's own credentials. It
  reaches cabal in a 0600 `CABAL_CONFIG` file removed after the upload, never
  on a command line; `cabal upload` has no environment variable for one.
- `package_diff` says the same statuses, reading PVP versions; `yanked` is a
  version Hackage marks deprecated, `drifted` compares the published sdist.
- Not done: `bump` leaves `.cabal` files alone (asked for by name it refuses;
  caught by a glob it says so and moves the crates), packages nested under a
  repo's `cabal.project` are not discovered, and a repo's `[publish]` step
  applies to its crates only.

`HYPERFORGE_CRATES_INDEX` / `HYPERFORGE_CRATES_DL` / `HYPERFORGE_HACKAGE_URL`
point the registry reads elsewhere (a `file://` directory serves as a registry
on disk; the tests use one). `HYPERFORGE_HACKAGE_URL` moves reads only, not an
upload. The one-repo forms `bump --org --name` and `package_diff --from_ref
--to_ref` are unchanged.

`publish --org --name --channel` publishes only with `--execute true`, on every
channel (HYPERFORGE-26; it used to publish at once). Without it, it runs the
channel's dry run, reads no token and says `publish_group` with phase `dry_run`:
`cargo publish --dry-run --allow-dirty`, `npm publish --dry-run`, or `twine
check dist/*` (twine has no dry-run upload, so that checks what is in `dist/`).

## The deploy-driver contract (`driver/`)

`hyperforge-driver` (HYPERFORGE-9) is the contract every deploy driver
implements and every caller uses: one bodiless `impl DriverActivation`
(`describe` · `trigger` · `watch` · `log` · `cancel` · `approve`), from which
plexus-macros emits `trait Driver` (a built-in driver implements it),
`DriverActivation<D>` / `Driver::server()` (serves any driver on a service;
`server` feature) and `DriverClient<T>: Driver` (a plugin over a transport).
`--no-default-features` is the wasm-safe build. A run ends in DEPLOY-10's
`Record` unless the system records itself (`Capabilities.records`). It takes
the plexus 0.6 line from crates.io, as the root crate does.

```bash
cargo test -p hyperforge-driver
cargo run -p hyperforge-driver --example toy_service -- 44190   # a scripted driver, for synapse -s / synapse-cc
synapse -P 44190 -s toy driver
```

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
- v4 is gone: `hyperforge-legacy`, `hyperforge-auth` and `hyperforge-ssh` are removed. The commit before HYPERFORGE-52 is the last with their source.

## Status

Production-ready for daily use. Twenty-five `V5PARITY-*` tickets shipped (see `plans/v5/V5PARITY/`); v4 features not carried over: release-asset upload, gitignore-sync, workspace check/verify. SSH keys are set per repo as `core.sshCommand` (`repos set_ssh_key`), in place of the `hyperforge-ssh` wrapper. Everything else from v4 is reachable.

## License

MIT
