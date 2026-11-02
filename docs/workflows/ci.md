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

## Does it build and pass: `[ci]` runners

A repo's `.hyperforge/config.toml` lists runners under `[ci]`, quickest first. By convention 0 is a quick check, 1 a full build, 2 the same in a container, and higher levels are custom gates (PERF-6 at 3 in plexus-core, plexus-macros and plexus-transport). Nothing to register: `ci run` reads them where they are.

```toml
[[ci.runners]]
type = "local"                     # or "docker": the repo is mounted at /workspace in `image`
build = ["cargo", "check"]         # joined with spaces, sh -c, in the repo
test = ["cargo", "test", "--lib"]
timeout_secs = 300                 # default 300; the step's process group is killed past it
```

```bash
hf ci run --path ~/dev/.../plexus-core                 # every local runner, build then test
hf ci run --path ~/dev/.../plexus-core --level 3       # one runner: the PERF-6 gate
hf ci run --name hypermemetic --level 0                # every member with [ci], a result table at the end
hf ci run --name hypermemetic --include 'plexus-*' --test false
hf ci run --name hypermemetic --level 2 --dry_run true # the steps, not run
```

A failed step stops that repo's later steps; the other repos still run. The run ends with `runner_summary` (its `table` has a row per repo: passed, failed with the step and exit, or skipped with why: no `[ci]`, `skip_validate`, no such level, no checkout) and with an error when anything failed, so a script sees a non-zero exit.
