# v5 architecture

## The three layers

```
                 wire
                   │
 ┌─────────────────┴─────────────────┐
 │  services  — translate events ↔ RPC   │   src/v5/{service,orgs,repos,workspaces}.rs
 └─────────────────┬─────────────────┘
                   │ typed outcomes
 ┌─────────────────┴─────────────────┐
 │  ops  — pure library + I/O        │   src/v5/ops/{state,repo,git,fs}.rs
 └─────────────────┬─────────────────┘
                   │ adapter trait
 ┌─────────────────┴─────────────────┐
 │  adapters — provider HTTP clients │   src/v5/adapters/{github,codeberg,gitlab}.rs
 └───────────────────────────────────┘
```

The split is enforced by V5LIFECYCLE-11's grep checkpoint and pinned by
**Decision D13**: services may not directly invoke `serde_yaml`,
`adapter.*`, `for_provider`, or `std::fs::{read,write,create_dir_all}`
against config paths. Every state read or write goes through `ops::`.

## ops:: submodules

Source: [`src/v5/ops/mod.rs`](../../src/v5/ops/mod.rs).

| Module | Owns |
|---|---|
| `ops::state` | YAML I/O for `config.yaml`, `orgs/*.yaml`, `workspaces/*.yaml`. `find_repo`, `find_repo_mut`, `delete_org_file`, `delete_workspace_file`. Re-exports `load_*` / `save_*` from `config.rs`. |
| `ops::repo` | `derive_provider`, `compute_drift`, `token_ref_for`, and the forge-call wrappers `exists_on_forge`, `create_on_forge`, `delete_on_forge`, `list_on_forge`, `read_metadata_on_forge`, `write_metadata_on_forge`, `privatize_on_forge`. Plus `sync_one` (the single per-repo sync primitive used by both `repos.sync` and `workspaces.sync`) and `dismiss` / `purge` (lifecycle state mutations). |
| `ops::fs` | `.hyperforge/config.toml` read/write at `<dir>/.hyperforge/config.toml`. The `HyperforgeRepoConfig` struct + `InitError`. |
| `ops::git` | `clone_repo`, `fetch`, `pull_ff`, `push_refs`, `status`, `is_dirty`, `set_remote_url`. All shell out to the user's `git` CLI so SSH agents / credential helpers / hooks flow through transparently. The only place in `src/v5/` that spawns `git` processes. |

A service method's job is to validate its inputs, call into `ops::`,
receive a typed outcome (a `Result<...>` over `ConfigError`,
`ForgePortError`, `GitError`, etc.), and translate that outcome into
its own `Event` envelope. No yaml, no adapter handle, no `std::fs`
inside the service body for state paths.

This DRY rule is regression-tested by V5LIFECYCLE-11:

```
grep -RE 'serde_yaml::from_str|serde_yaml::to_string|fs::(read_to_string|write)' src/v5/ \
  | grep -v '^src/v5/(ops|secrets)/'                       # → empty
grep -RE 'adapter\.(read_metadata|write_metadata|create_repo|delete_repo|repo_exists|update_repo)' src/v5/ \
  | grep -v '^src/v5/ops/'                                  # → empty
grep -RE 'for_provider' src/v5/ | grep -v '^src/v5/(ops|adapters)/'   # → empty
grep -RE 'compute_drift' src/v5/ | grep -v '^src/v5/ops/'             # → empty
```

Any non-empty match fails the checkpoint.

## ForgePort

Source: [`src/v5/adapters/mod.rs`](../../src/v5/adapters/mod.rs).

`ForgePort` is the portable capability trait every adapter implements.
Its surface is the **D3-revised-by-D10 intersection**:

```rust
async fn read_metadata(&self, remote, repo_ref, auth) -> Result<ForgeMetadata, ForgePortError>;
async fn write_metadata(&self, remote, repo_ref, fields, auth) -> Result<MetadataFields, ForgePortError>;
async fn create_repo(&self, remote, repo_ref, visibility, description, auth) -> Result<(), ForgePortError>;
async fn delete_repo(&self, remote, repo_ref, auth) -> Result<(), ForgePortError>;
async fn repo_exists(&self, remote, repo_ref, auth) -> Result<bool, ForgePortError>;
async fn list_repos(&self, org, auth) -> Result<Vec<RemoteRepo>, ForgePortError>;
```

Portable metadata fields are exactly four: `default_branch`,
`description`, `archived`, `visibility`. Provider-specific fields
(GitHub topics, GitLab namespaces, etc.) MAY be read internally by an
adapter but MUST NOT leak through this trait.

### Error classes

`ForgeErrorClass` is a closed set: `not_found`, `auth`, `network`,
`unsupported_field`, `rate_limited`, `conflict`,
`unsupported_visibility`. Every adapter error maps into one of these
seven; tests assert on the class, not the message.

### Authentication

```rust
pub struct ForgeAuth<'a> {
    pub token_ref: Option<&'a str>,    // a `secrets://...` ref
    pub resolver: &'a dyn SecretResolver,
}
```

Adapters resolve the secret at call-time through the `SecretResolver`
trait. The plaintext never leaves the adapter; no method that returns
`OrgDetail` (or anything else carrying a `CredentialEntry`) ever
includes resolved values. This is the **secret redaction rule** from
CONTRACTS §types.

### Adding an adapter

1. New file at `src/v5/adapters/<provider>.rs`.
2. Implement `ForgePort` for the new struct. Any provider field your
   adapter needs that isn't in the four portable ones lives as a
   private struct member; it MUST NOT appear in any trait return type.
3. Register the dispatch in `for_provider` (in
   `src/v5/adapters/mod.rs`). That function is the only place outside
   `src/v5/ops/` allowed to call it (per the DRY grep).
4. Add a `ProviderKind` variant if it's a fully new forge (not a
   GitHub-/Gitea-/GitLab-compatible one).

`for_provider` is dynamically dispatched (`Box<dyn ForgePort>`); the
trait is `Send + Sync`.

## SecretResolver

Source: [`src/v5/secrets.rs`](../../src/v5/secrets.rs).

```rust
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError>;
}
```

`SecretRef` parses to validate the `secrets://<non-empty path>` shape;
construction rejects malformed refs before any I/O.

The v1 backend is `YamlSecretStore`, reading `<config_dir>/secrets.yaml`
on every call (no in-memory cache, so edits are picked up without
restarting the daemon). Future backends (OS keyring, remote KMS) plug
in as additional `SecretResolver` implementations.

`SecretError` is a closed set with a `code()` method that surfaces a
snake_case wire discriminator: `invalid_ref`, `not_found`,
`corrupt_store`, `bad_value`, `io_error`. The `BadValue` variant
catches non-string entries in the yaml — keys present but with a
non-scalar value.

V5PARITY-7 will land `secrets.set` / `secrets.delete` / `secrets.list_refs`
methods at the wire boundary; the `YamlSecretStore` already ships
`put_secret`, `delete_secret`, `list_refs` in support. Until then,
hand-edit `secrets.yaml`.

## How a method is added

The pattern is consistent across the services (see `repos.delete` for a
typical example):

1. **Define the event variant** on the service's `Event` enum, with
   `#[serde(tag = "type", rename_all = "snake_case")]`. New methods
   that introduce a payload shape should add a distinct variant — a
   different `type` discriminator — rather than overloading an
   existing one. Test scripts assert on `.type == "<category>"`.
2. **Add the method** under the `#[plexus_macros::activation(...)]`
   block on the service. Use `#[plexus_macros::method(params(...))]` to
   document each parameter.
3. **Validate at the entry**. Empty strings, malformed shapes, missing
   required fields — every one of these is a typed `Error` event
   yielded immediately, never a panic or a silent default.
4. **Call into `ops::`**. If the operation needs YAML state, go
   through `ops::state`. If it needs forge metadata, go through
   `ops::repo` (which dispatches via `for_provider` internally). If
   it needs git, go through `ops::git`. If it needs
   `.hyperforge/config.toml`, go through `ops::fs`.
5. **Translate the outcome to events**. Per-remote operations emit
   per-remote events plus a final aggregate. Whole-repo operations
   typically emit one main event plus an ack — e.g. `repos.add`
   emits `repo_detail` + `repo_added`, with `repo_created` first
   when `create_remote=true`.
6. **Honor the standard parameters**. `dry_run` defaults `false` per
   D7; on `dry_run=true` no disk write or forge mutation happens but
   the event stream shape is identical to a real run.

The activation tree itself is **static-only** in v1: no
`#[child(list=...)]`, no dynamic children. A new namespace requires a
new `Service` struct and a `#[plexus_macros::child]` accessor on the
parent. The upgrade path to dynamic children (plexus-macros 0.5+) is
additive and deferrable.
