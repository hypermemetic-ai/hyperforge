---
id: V5LIFECYCLE-4
title: "ops::repo::{exists,create,delete}_on_forge — the forge-call wrappers"
status: Complete
type: implementation
blocked_by: [V5LIFECYCLE-3]
unlocks: [V5LIFECYCLE-5, V5LIFECYCLE-6, V5LIFECYCLE-7]
---

## Problem

`repos.add --create_remote` (V5PROV-6) and `workspaces.sync`'s auto-create path (V5PROV-8) both do the same dance: resolve provider → construct ForgeAuth → call `adapter.create_repo`. Similarly for the check-exists path (`repo_exists`) and the delete path (`delete_repo`). Per D13, one wrapper each, called from every site.

## Required behavior

Three pure-ish functions in `src/v5/ops/repo` (signatures are the contract; names indicative):

| Function | Inputs | Output |
|---|---|---|
| `exists_on_forge` | `&OrgRepo`, `&OrgConfig`, `&BTreeMap<DomainName, ProviderKind>`, `&dyn SecretResolver`, `Option<&RemoteUrl>` (filter) | `Result<bool, ForgePortError>` (or a richer `ExistsOutcome` if multi-remote merging is needed) |
| `create_on_forge` | same + `ProviderVisibility` + `description: &str` | `Result<(), ForgePortError>` |
| `delete_on_forge` | same | `Result<(), ForgePortError>` |

Each resolves provider via `ops::repo::derive_provider` (already pub(crate) since V5WS-9), resolves credentials via the resolver, constructs the adapter via `crate::v5::adapters::for_provider`, and calls the corresponding trait method. No event emission — just Result.

Migration:
- `ReposService::add --create_remote` path (V5PROV-6) calls `ops::repo::exists_on_forge` + `ops::repo::create_on_forge`.
- `WorkspacesService::sync` auto-create path (V5PROV-8) calls the same two.
- `ReposService` `delete`-like code paths (after V5LIFECYCLE-5/6/7 land) call `ops::repo::delete_on_forge` from V5LIFECYCLE-7.
- No service invokes `for_provider` or `adapter.{create_repo,delete_repo,repo_exists}` directly anywhere outside `ops::`.

## What must NOT change

- V5PROV-6's `repos.add --create_remote` externally-visible behavior (event stream, rollback semantics).
- V5PROV-8's `workspaces.sync` auto-create behavior (`status: created` events, `created` counter).
- ForgePort trait surface — this ticket consumes existing methods, doesn't add any.

## Acceptance criteria

1. Tier-1 sweep passes green; counts identical to pre-ticket.
2. `grep -RE 'adapter\.(create_repo|delete_repo|repo_exists)|for_provider' src/v5/{repos,workspaces,orgs,service}.rs` returns empty — all callers route through `ops::repo::*`.
3. The `V5PROV-6` tier-2 test (with tier-2 config populated) still passes end-to-end after the refactor.
4. The `V5PROV-8` tier-2 test still passes end-to-end after the refactor.

## Completion

- Run `bash tests/v5/V5LIFECYCLE/V5LIFECYCLE-4.sh` → exit 0 (tier 1: grep-based invariant + a trivial end-to-end against a mocked "provider" if one exists, else tier 1 just runs the grep).
- Status flips in-commit.
