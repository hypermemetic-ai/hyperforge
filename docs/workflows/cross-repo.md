# Cross-repo development

Manifests name published versions, never `path = "../sibling"`: every repo builds from a fresh clone. To build one checkout against your local copies of other crates, link them:

```bash
hf build link --name hypermemetic --path ~/dev/.../menger --include plexus-core,plexus-transport
cargo test                      # in menger: uses the sibling checkouts
hf build unlink --path ~/dev/.../menger
```

`link` writes `.cargo/config.toml` in that checkout, pointing the named crates at their sibling checkouts, and excludes it from git locally; it refuses if that file is tracked or not its own. Nothing committed changes. `unlink` removes it.

When the change is ready, release the dependencies first ([bump](bump.md), [publish](publish.md)), then move the consumer to the new versions.
