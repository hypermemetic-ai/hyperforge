---
id: ORGS-2
title: "`orgs_add` RPC with dry-run and validation"
status: Complete
type: implementation
blocked_by: []
unlocks: [ORGS-4]
---

## Problem

Users cannot register a new org without hand-editing
`~/.config/hyperforge/orgs/<org>.toml` and calling `reload`. The existing
`orgs_delete` method already demonstrates the write path: it calls
`OrgConfig::save()` as part of its SSH cleanup branch. The inverse
operation — creating the file in the first place — is missing.

## Context

`OrgConfig` struct (`src/config/org.rs:12-21`):

```rust
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct OrgConfig {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ssh: HashMap<String, String>,        // forge → ssh key path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
}
```

`OrgConfig::save()` (`src/config/org.rs:39-50`) accepts the org name,
creates the directory if needed, and writes TOML.

`HyperforgeConfig::parse_forge()` (`src/service.rs:259`) is the canonical
validator for forge names (`github`, `codeberg`, `gitlab`).

`orgs_delete` (`src/service.rs:967-1049`) is the closest analog — same file,
same event stream style, already has a dry-run flag.

## Required behavior

### Capability: typed, closed-set forge keys

The `ssh` parameter is a typed record with one optional field per known forge. The forge set is closed (`github`, `codeberg`, `gitlab` today). Two consequences follow from the type, not from any service-level validation:

- Synapse emits discoverable per-forge CLI flags (one per field). No JSON-blob flag.
- Unknown forges are rejected at CLI parse time, before the call reaches the service.

When the closed set grows (e.g. a future `sourcehut`), the record grows a field. That's a deliberate typed change; dynamic forge names are not supported.

The parameter is optional. An absent parameter results in an org with no SSH keys.

### Capability: observable success and failure

The method emits events on a stream. Each case below must be distinguishable from the others by inspecting only the event stream — a test must not need to read service internals to tell outcomes apart.

| Input | Outcome a caller can observe |
|---|---|
| `org` is empty, or contains `/`, `..`, `\0`, or a path separator | A failure event is emitted; the event identifies the failure as an invalid-name case and echoes the offending value. No file is written. |
| An org config already exists for the given `org` | A failure event is emitted; the event identifies the failure as an already-exists case and names the org. The existing file is not modified (mtime unchanged). |
| All inputs valid, `dry_run: true` | A success event is emitted indicating no write occurred; the event identifies the call as a preview and names the org. No file is written. |
| All inputs valid, `dry_run: false` or omitted | A success event is emitted identifying the write as a creation; the event names the org. An org config with the provided fields is now persisted and is visible to `orgs_list`. |

"Identifies the failure as invalid-name" / "as already-exists" etc. is a capability constraint, not a string contract — the test distinguishes cases by whatever discriminator the implementer chooses (variant, code, message content), but a test MUST be able to distinguish them without ambiguity.

### Capability: round-trip integrity

Calling `orgs_add` with given `ssh` and `workspace_path`, then calling `orgs_list`, returns the same field values. The intermediate on-disk shape is an implementation detail.

## What must NOT change

- `orgs_list` behavior (including its ordering and fields).
- `orgs_delete` behavior.
- `OrgConfig` struct shape. This ticket adds no fields.
- The root service's existing method set. Only addition, no rename or
  removal.
- `reload` still works and picks up the new org on its next call.

## Acceptance criteria

1. `synapse -P 44104 lforge hyperforge orgs_add --org newtest --ssh.github ~/.ssh/gh --dry_run true` succeeds, and no new org config exists in `~/.config/hyperforge/orgs/` after the call.
2. Running the same command with `--dry_run false` persists a new org config and `orgs_list` includes `newtest` with the provided `ssh.github` value.
3. `synapse … orgs_add --org newtest --dry_run false` run a second time fails; the previously-created config is unchanged (mtime unchanged), and a caller observing only the event stream can identify the failure as "already exists" without reading source code.
4. `synapse … orgs_add --help` lists one discoverable flag per known forge (one each for github, codeberg, gitlab). There is no single flag that accepts a JSON object for ssh.
5. Attempting `synapse … orgs_add --org newtest --ssh.not-a-forge …` fails at the CLI parse layer (before the call reaches the service). No org config is created.
6. `synapse … orgs_add --org ../escape --dry_run true` fails with an invalid-name diagnosis observable from the event stream; no file is written outside the managed orgs directory.
7. Every failure case above is distinguishable from every other failure case by inspecting only the event stream — a verifier can tell "invalid name" from "already exists" from "parse rejection" without reading the service implementation.
8. Round-trip integrity: for any ssh / workspace_path values passed to `orgs_add`, the subsequent `orgs_list` returns the same values.
9. Integration test in `tests/integration_test.rs` (new `test_orgs_add_*` cases) covers every case above. `cargo test --test integration_test` passes.

## Completion

- Method added to `src/service.rs` under the root service activation, marked
  `#[plexus_macros::method(description = "…", params(…))]`, mirroring the
  style of `orgs_delete`.
- Integration tests for this method added.
- Documentation owned by this ticket is updated in the same commit:
  - `README.md` cheatsheet gains an `orgs_add` row with a one-line example.
  - `~/.claude/skills/hyperforge/SKILL.md` cheatsheet gains the same.
  - Any prior claim of "no orgs_add RPC" in `AUTH_IMPLEMENTATION.md` gets
    a note pointing to the new method.
- All acceptance criteria pass from the command line on a clean
  workstation.
- Status flipped to `Complete` in the same commit.
