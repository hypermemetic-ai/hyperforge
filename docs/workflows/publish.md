# Publish

## A chain of crates and Cabal packages

From the workspace, by package name:

```bash
hf build package_diff --name hypermemetic --include menger                 # what is unpublished
hf build publish --name hypermemetic --include menger                      # dry run of everything menger needs
hf build publish --name hypermemetic --include menger --execute true       # for real
```

It takes what the included packages need first (`--closure false`: only them), skips versions the registry holds, and groups the rest in dependency order. It dry-runs every group first, each against the others' checkouts, and publishes nothing unless all pass. With `--execute true` it publishes group by group, stops at the first failure naming what is and is not published, and tags each `<package>-v<version>` locally. Push the tags yourself. A glob like `plexus-*` catches crates and Cabal packages: read the plan.

Logs: `~/.local/state/hyperforge/publish/<time>/`.

A repo with its own publish step declares it in `.hyperforge/config.toml`:

```toml
[publish]
crates = ["menger-tickets-api", "menger"]
dry_run = "scripts/publish.sh dry-run"
execute = "scripts/publish.sh publish"
```

## One repo, any channel

```bash
hf build publish --org hypermemetic-ai --name plexus-protocol --channel hackage              # dry run
hf build publish --org hypermemetic-ai --name plexus-protocol --channel hackage --execute true
```

Channels: `crates.io`, `hackage`, `npm`, `pypi`. Without `--execute true` it only runs the channel's dry run.

## Tokens

`secrets://cargo/token`, `secrets://hackage/token`, `secrets://npm/token`, `secrets://pypi/token` (see [secrets](secrets.md)). A token reaches the tool through its environment or a 0600 file removed afterwards, never a command line. For crates.io, cargo's own credential provider (e.g. `pass`) wins when configured.
