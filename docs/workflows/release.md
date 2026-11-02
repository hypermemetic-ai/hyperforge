# Release one repo

```bash
hf build release --org hypermemetic-ai --name plexus-transport --bump patch                       # the plan
hf build release --org hypermemetic-ai --name plexus-transport --bump patch --execute true \
   --assets target/dist/tool-x86_64.tar.gz,target/dist/tool-aarch64.tar.gz --channel crates.io
```

In order: refuse a dirty tree or a branch not level with origin; bump; commit; annotated tag `v<version>`; push the branch and the tag; attach `--assets` to the forge release (created if missing); publish to `--channel` if given. Each step that fails says what already happened. It does not build binaries: build them first. Asset paths are relative to the checkout.

Changelog: edit it before releasing; hyperforge does not write one.

Attach files to an existing tag:

```bash
hf build release_asset --path ~/dev/.../tool --tag v1.2.0 --files dist/a.tar.gz,dist/b.tar.gz
hf build release_asset --path ~/dev/.../tool --tag v1.2.0 --files dist/a.tar.gz,dist/b.tar.gz --execute true
```

It checks each file (size, sha256) and the tag on the forge first. An asset of the same name is refused unless `--replace true`; a failed replace leaves the old asset deleted, and says so.

A coordinated tag across many repos: `hf workspaces tag --help`. A crate chain: [publish](publish.md).
