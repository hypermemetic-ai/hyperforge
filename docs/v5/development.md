# Developing on v5

This is for someone hacking on `src/v5/`. For user-facing docs see
[getting-started](./getting-started.md).

## Build

```bash
cargo build --bin hyperforge-v5            # debug
cargo build --bin hyperforge-v5 --release  # release
```

The v5 binary is one of four binaries in the same crate; v4 keeps
building unchanged. Workspace deps live on crates.io as
`plexus-core = "0.5"`, `plexus-macros = "0.5"`, `plexus-transport =
"0.2"`.

## Test harness

Source: [`tests/v5/harness/lib.sh`](../../tests/v5/harness/lib.sh).
Contract: [CONTRACTS §harness](../../plans/v5/CONTRACTS.md).

Tests run against a real daemon spawned on an ephemeral port. The
assertion surface is the synapse RPC stream — bash invocations of
`synapse -P $HF_PORT --json lforge-v5 hyperforge ...` parsed with
`jq`. Rust runs each script as one `#[test]` via the integration
runner at `tests/v5_integration.rs`.

### Writing a test script

```bash
#!/usr/bin/env bash
# tier: 1
set -euo pipefail
source "$(dirname "$0")/../harness/lib.sh"

hf_spawn
hf_load_fixture minimal_org
hf_cmd orgs list | hf_assert_event '.type == "org_summary" and .name == "demo"'
```

Scripts live under `tests/v5/<EPIC>/<TICKET>.sh`. Line 2 self-declares
the tier as a magic comment (`# tier: 1`, `2`, or `3`; default 1).

### Helpers

| Function | Behavior |
|---|---|
| `hf_spawn` | Spawns daemon on an ephemeral port. Exports `$HF_PORT`, `$HF_CONFIG` (temp dir). Registers `hf_teardown` on EXIT. |
| `hf_cmd <args...>` | Runs `synapse -P $HF_PORT --json lforge-v5 hyperforge <args>`. Stdout is the unwrapped NDJSON event stream. |
| `hf_load_fixture <name>` | Copies `tests/v5/fixtures/<name>/` over `$HF_CONFIG`. |
| `hf_put_secret <ref> <value>` | Writes a secret into `$HF_CONFIG/secrets.yaml`. |
| `hf_add_provider_map <domain> <provider>` | Appends a `provider_map:` entry to `config.yaml`. |
| `hf_assert_event <jq filter>` | Reads stdin (event stream); fails if zero events match. |
| `hf_assert_no_event <jq filter>` | Same, but fails if any event matches. |
| `hf_assert_count <jq filter> <n>` | Asserts exactly `n` matches. |
| `hf_require_tier2 [<forge>]` | SKIP-clean exit when `$HF_V5_TEST_CONFIG_DIR` is unset / missing `tier2.env`. |
| `hf_use_test_config` | Overlays `$HF_V5_TEST_CONFIG_DIR/` (minus `tier2.env`) onto `$HF_CONFIG`. |

### Tiers

| Tier | What | How |
|---|---|---|
| 1 | Offline. No real forge contact, no network. | `cargo test --test v5_integration` |
| 2 | Live forge. Hits real GitHub/Codeberg/GitLab APIs against a disposable test repo. | `cargo test --test v5_integration --features tier2` (also requires `HF_V5_TEST_CONFIG_DIR` pointing at a config dir with valid creds + a `tier2.env`) |
| 3 | Orchestrated. Cross-forge + workspace-scale. | `cargo test --test v5_integration --features tier3` |

### `HF_V5_TEST_CONFIG_DIR`

Tier-2 tests read this env var. It points at a user-owned directory
that mirrors `~/.config/hyperforge/`:

```
$HF_V5_TEST_CONFIG_DIR/
├── config.yaml             # provider_map + (optional) default_workspace
├── orgs/<org>.yaml         # real orgs with `secrets://...` cred refs
├── secrets.yaml            # real token / SSH-key values
└── tier2.env               # bash-sourceable test target params:
                            #   HF_TIER2_GITHUB_ORG=...
                            #   HF_TIER2_GITHUB_REPO=...
                            #   HF_TIER2_CODEBERG_ORG=...
                            #   ...
```

Everything except `tier2.env` is format-identical to production config.
`hf_use_test_config` overlays it onto the spawned daemon's `$HF_CONFIG`
so the run uses real credentials against the disposable repo named in
`tier2.env`.

## DRY invariants (D13 / V5LIFECYCLE-11)

The core architectural rule: **services do not touch state directly**.
Every YAML read/write, every `ForgePort` adapter call, every
`.hyperforge/config.toml` interaction goes through `src/v5/ops/`.

V5LIFECYCLE-11's checkpoint asserts this with grep:

```bash
grep -RE 'serde_yaml::from_str|serde_yaml::to_string|fs::(read_to_string|write)' src/v5/ \
  | grep -v '^src/v5/(ops|secrets)/'                       # → empty
grep -RE 'adapter\.(read_metadata|write_metadata|create_repo|delete_repo|repo_exists|update_repo)' src/v5/ \
  | grep -v '^src/v5/ops/'                                  # → empty
grep -RE 'for_provider' src/v5/ | grep -v '^src/v5/(ops|adapters)/'   # → empty
grep -RE 'compute_drift' src/v5/ | grep -v '^src/v5/ops/'             # → empty
```

Any non-empty match is a regression. If you find yourself reaching for
`serde_yaml` or `adapter.something()` from inside a service method, that's
a sign you should add a function under `src/v5/ops/<module>.rs` and
call that.

## The service-method-to-ops translation pattern

A typical method body:

```rust
#[plexus_macros::method(params(org = "...", name = "...", dry_run = "..."))]
pub async fn whatever(
    &self, org: String, name: String, dry_run: Option<Value>,
) -> impl Stream<Item = RepoEvent> + Send + 'static {
    let config_dir = self.config_dir.clone();
    stream! {
        // 1. validate inputs (typed Error events on failure)
        let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
        if org.is_empty() { yield validation_event("..."); return; }

        // 2. read state through ops::state
        let loaded = match crate::v5::ops::state::load_all(&config_dir) {
            Ok(l) => l,
            Err(e) => { yield cfg_error_event(e); return; }
        };
        let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
            yield not_found_event(...); return;
        };

        // 3. call into ops::repo for forge / drift / lifecycle work
        let outcomes = crate::v5::ops::repo::sync_one(...).await;

        // 4. translate outcomes → RPC events
        for o in outcomes {
            yield RepoEvent::SyncDiff { ... };
        }
    }
}
```

Validate at the top, route through `ops::*`, translate at the bottom.
That's the entire shape.

## Cadence — one-pass implementation, verify at end

The project methodology is "tickets pin capabilities, not
implementations" combined with one-pass implementation: when you pick
up a `Ready` ticket, do the whole thing without iterating between
"write a bit, run tests, write more, run tests." Implement the entire
ticket cleanly against the contract, then run the verification at the
end (the ticket's `Run bash tests/v5/<EPIC>/<TICKET>.sh` line).

This works because tickets pin external contracts (parameter names,
event shapes, error classes, file paths). Internal struct bodies,
module placements, and derive lists are the implementer's call.

## Ticketing methodology

All v5 tickets start `status: Pending`. Only the user promotes to
`Ready`. No implementation begins until promotion. The
`epic-evaluation` skill runs per-epic as the promotion gate — it
catches illustrative-not-pinned types, soft acceptance criteria, and
hidden design decisions before any cycle is burned.

If you're drafting a new epic or ticket, see the project-level
ticketing/planning skills (`process_ticketing-methodology` and
related) plus the existing epic READMEs at `plans/v5/V5*/V5*-1.md`
for shape.

## Source map

| Path | Owns |
|---|---|
| `src/v5/service.rs` | Root `HyperforgeService`, `status`, `resolve_secret`, child accessors |
| `src/v5/orgs.rs` | `OrgsService` — CRUD + credentials |
| `src/v5/repos.rs` | `ReposService` — CRUD, lifecycle, sync/push, import, git transport |
| `src/v5/workspaces.rs` | `WorkspacesService` — CRUD, reconcile, sync, discover, workspace-parallel git |
| `src/v5/config.rs` | YAML schema types (`OrgConfig`, `WorkspaceConfig`, etc.) + load/save |
| `src/v5/secrets.rs` | `SecretRef`, `SecretResolver`, `YamlSecretStore` |
| `src/v5/adapters/` | `ForgePort` trait + GitHub/Codeberg/GitLab adapters |
| `src/v5/ops/state.rs` | YAML I/O facade for services |
| `src/v5/ops/repo.rs` | provider derivation, drift compute, forge-call wrappers, sync_one, lifecycle mutations |
| `src/v5/ops/git.rs` | git CLI subprocess wrappers |
| `src/v5/ops/fs.rs` | `.hyperforge/config.toml` read/write |
| `src/bin/hyperforge-v5.rs` | Daemon entry point |
| `tests/v5/harness/lib.sh` | Test harness contract |
| `tests/v5/<EPIC>/<TICKET>.sh` | Per-ticket integration tests |
| `plans/v5/README.md` | Roadmap + design invariants |
| `plans/v5/CONTRACTS.md` | Types, decisions D1..D14, harness surface |
