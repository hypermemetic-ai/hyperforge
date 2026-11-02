# CI

A repo declares pipelines in `.hyperforge/pipelines.toml`; register the repo once and each pipeline appears under `ci`:

```toml
[[pipeline]]
name = "menger-app"

[[pipeline.stage]]
name = "build"
run = "deploy/pipeline image"      # sh -c, in the repo
trigger = "auto"                   # runs when a run starts; "manual" is the default
outputs = ["image"]                # key=value lines written to $HF_OUTPUTS

[[pipeline.stage]]
name = "deploy-local"
needs = ["build"]                  # its outputs arrive as $IMAGE
run = "deploy/pipeline local --image \"$IMAGE\""
```

```bash
hf ci add --path ~/dev/.../menger
hf ci pipelines
hf ci menger-app start                             # a run; auto stages start
hf ci menger-app trigger --run 7 --stage deploy-local
hf ci menger-app watch --run 7 --follow true
hf ci menger-app log --run 7 --stage build
hf ci menger-app cancel --run 7 --stage deploy-local
```

A non-zero exit, a missing output or a timeout fails the stage. Runs and logs live in `~/.local/state/hyperforge/ci/`; a stage running when hyperforge restarts is marked failed.

A shell command in every member, outside any pipeline: `hf build foreach --name hypermemetic --cmd 'cargo check'`.

Per-repo `[ci]` runners in `.hyperforge/config.toml` are not run yet (HYPERFORGE-57).
