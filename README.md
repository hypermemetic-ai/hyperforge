# Hyperforge

Hyperforge runs the workspace's repos as one: status across every checkout,
ticket worktrees, local cross-repo development, version bumps, releases,
publishing to crates.io, Hackage and npm, release assets, pull requests and
pipelines. It is a daemon; you talk to it with `synapse`.

These docs cover **workflows**. The API documents itself: every namespace and
method lists its parameters and what it does.

```bash
alias hf='synapse -P 44104 lforge-v5 hyperforge'
hf                       # namespaces and root methods
hf build                 # a namespace's methods
hf build bump --help     # one method's parameters
```

Nothing that writes runs without asking for it: methods that change a remote,
a registry or many repos plan by default and act with `--execute true`, or
take `--dry-run true`. Read the plan first.

## Workflows

| I want to | Read |
|---|---|
| set up a new machine or org | [onboarding](docs/workflows/onboarding.md) |
| see where every repo stands | [orient](docs/workflows/orient.md) |
| start work on a ticket | [start a ticket](docs/workflows/ticket.md) |
| change crates in several repos at once | [cross-repo development](docs/workflows/cross-repo.md) |
| move versions | [bump](docs/workflows/bump.md) |
| cut a release of one repo, with files attached | [release](docs/workflows/release.md) |
| publish crates, Cabal packages or npm packages | [publish](docs/workflows/publish.md) |
| open a pull request | [pull requests](docs/workflows/pr.md) |
| push to every forge a repo lives on | [mirroring](docs/workflows/mirroring.md) |
| run pipelines | [CI](docs/workflows/ci.md) |
| list, push or delete container images | [images](docs/workflows/images.md) |
| store a token | [secrets](docs/workflows/secrets.md) |

## Install and run

```bash
cargo build --release --bin hyperforge
install -m755 "${CARGO_TARGET_DIR:-target}/release/hyperforge" ~/.local/bin/hyperforge
ln -sf hyperforge ~/.local/bin/hyperforge-v5          # the name the service unit runs
install -Dm644 deploy/systemd/hyperforge.service ~/.config/systemd/user/hyperforge.service
systemctl --user daemon-reload && systemctl --user enable --now hyperforge
hf status                                             # answers on 44104
journalctl --user -u hyperforge -f                    # its log
```

After installing a new binary: `systemctl --user restart hyperforge`.

`synapse` is `plexus-synapse` from Hackage. It must be on `PATH`
(`~/.local/bin` is not on a non-login ssh shell's) and wants a UTF-8 locale:
`export LANG=C.UTF-8`.

## Where things live

```
~/.config/hyperforge/
├── config.yaml              provider_map, default_workspace
├── secrets.yaml             secrets://… (0600)
├── orgs/<org>.yaml          an org: forge, credentials, repos
├── workspaces/<name>.yaml   a workspace: its path and members
└── ci.yaml                  repos whose pipelines are registered
~/.local/state/hyperforge/   publish logs, CI runs
<repo>/.hyperforge/config.toml     a repo's org and forges (name defaults to its directory)
<repo>/.hyperforge/pipelines.toml  its pipelines
```

## Also in this repo

- `driver/`: `hyperforge-driver`, the deploy-driver contract (`cargo doc -p hyperforge-driver`).
- `vault/`: the secrets vault (`vault/README.md`).
- `scripts/auth-walk.py`: walks every token hyperforge still needs; `scripts/bootstrap.sh`: zero to cloned on a new machine.

## License

See `Cargo.toml`.
