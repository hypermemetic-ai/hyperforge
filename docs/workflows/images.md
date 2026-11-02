# Container images

An org's images live on its forge's registry: `ghcr.io/<org>/<name>` for GitHub, `codeberg.org/<org>/<name>` for Codeberg. The org's yaml picks the registry; `--forge github|codeberg` overrides it.

See what is there:

```bash
hf images list_all --org hypermemetic-ai                    # every image the org has
hf images list --org hypermemetic-ai --name menger          # its versions: digest and every tag each carries
hf images list --org hypermemetic-ai --name menger --filter 2026
```

Push one:

```bash
hf images push --org hypermemetic-ai --name tool --tag v1.2.0 --also-tags latest --path ~/dev/.../tool                  # the plan
hf images push --org hypermemetic-ai --name tool --tag v1.2.0 --also-tags latest --path ~/dev/.../tool --execute true   # build, push
hf images push --org hypermemetic-ai --name tool --tag v1.2.0 --image tool:local --execute true                       # one built already
```

`--path` builds the directory's `Dockerfile` (or `Containerfile`, `docker/Dockerfile`, or `--dockerfile`). `--image` tags and pushes an image already built on this machine. It runs `docker` (`HF_CONTAINER_CLI` names another). The push signs in with the org's token from the secret store, through a throwaway client config that is deleted afterwards, so `~/.docker/config.json` is untouched; with no token, it says so and uses Docker's own stored login. A failure part way names what was pushed and what was not.

Delete one:

```bash
hf images delete --org hypermemetic-ai --name tool --tag v1.1.0                    # what would go
hf images delete --org hypermemetic-ai --name tool --tag v1.1.0 --execute true
hf images delete --org hypermemetic-ai --name tool --tag sha256:... --execute true  # an untagged version
```

On GitHub a tag names a version, and deleting it deletes every tag that version carries: `latest` is usually the same image as a release tag. That is refused, naming the other tags, unless `--with-other-tags true`.

Tokens: listing needs `read:packages`, pushing `write:packages`, deleting `delete:packages`. hyperforge looks for `secrets://github/<org>/packages_token` first, then the org's forge token. A forge token without those scopes gets a 403 that says which scope is missing. To store one: [secrets](secrets.md).
