# Mirror to every forge

A repo registered on more than one forge (its org yaml lists a remote on each) can be pushed to all of them at once.

```bash
hf repos mirror --name plexus-core                       # the plan: each forge, refspecs, which credential
hf repos mirror --name plexus-core --status true         # what each forge has: branch same/missing/differs, tags missing
hf repos mirror --name plexus-core --execute true        # push
hf repos mirror --workspace hypermemetic                 # every member, planned
```

What is pushed: the checkout's current branch (or `--branches a,b`, or `--all-branches true`) and every tag. Never forced: a forge whose branch or tag has moved on refuses that ref, the others still go, and the run ends with an `error` naming each forge that failed. Targets come from the org yaml, so no extra git remote is needed in the checkout.

## Set one up

1. Register the second forge's URL on the repo: `hf repos add_remote --org ORG --name REPO --remote '{"url":"https://codeberg.org/OWNER/REPO.git"}'`. Create the repo on that forge first if it does not exist.
2. Give the forge a token, if pushing over HTTPS: `hf secrets set --key secrets://codeberg/OWNER/token --value -` (from stdin). SSH remotes use your SSH agent instead.
3. If the token is not at `<forge>/<owner>/token` or `<forge>/_default/token`, name it on the org, beside the primary `forge:` block:

```yaml
mirrors:
- provider: codeberg
  credentials:
  - key: secrets://codeberg/my-mirror/token
    type: token
```

The plan (`hf repos mirror --name REPO`) shows which reference each forge will use before anything is pushed. Tokens reach git through an inline credential helper that reads the environment, never a command line or a file.
