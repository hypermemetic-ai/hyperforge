# Bump

Across the workspace, by crate (globs on crate names, not repos):

```bash
hf build bump --name hypermemetic --include plexus-core --bump minor --dry-run true
hf build bump --name hypermemetic --include plexus-core --bump minor
```

It sets each included crate's version, moves every dependent requirement that matched the old version, refreshes each repo's `Cargo.lock`, and commits each repo once (`--commit false` leaves the edits). It refuses to write over an uncommitted manifest. `--closure true` also bumps what the included crates need; `--to 0.7.1` sets an exact version.

One repo: `hf build bump --org hypermemetic-ai --name plexus-transport --bump patch --dry-run true`.

The plan lists the requirements it moves. A consumer pinned to an older line through the registry is left alone and named, so you can decide whether to move it.

`.cabal` versions are not bumped yet: edit them by hand.
