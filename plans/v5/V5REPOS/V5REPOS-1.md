---
id: V5REPOS-1
title: "Hyperforge v5 Repos — CRUD, ForgePort, Adapters"
status: Epic
type: epic
blocked_by: [V5CORE-3, V5CORE-4, V5CORE-7, V5CORE-9]
unlocks: [V5WS-9]
---

## Goal

Populate the `ReposService` stub (registered in V5CORE-7) with full repo CRUD
against the org-YAML storage, plus the `ForgePort` trait and adapters for
GitHub, Codeberg, and GitLab, plus the `sync` and `push` methods that
carry repo metadata between local state and remote forges through those
adapters.

When this epic is done:

- Repo lifecycle (`list`, `get`, `add`, `remove`, `add_remote`,
  `remove_remote`) operates against `~/.config/hyperforge/orgs/<org>.yaml`
  — never via hand-edit.
- Remote URLs are mapped to providers via the global provider-domain map
  in `config.yaml` (pinned in V5CORE-3), with a per-remote `provider:`
  override for custom instances.
- `ForgePort` is a single trait that defines the capability adapters
  implement — enough for the v5 scope (sync/push metadata; no git
  transport operations).
- Three adapters (`github`, `codeberg`, `gitlab`) implement `ForgePort`
  and resolve their credentials through V5CORE-4's secret resolver,
  reading from the org's credentials list.
- `repos.sync <org> <name>` reads current metadata from the remote via
  the matched adapter and returns a diff-to-local summary; `repos.push
  <org> <name>` applies local metadata to the remote.

## Dependency DAG

```
           V5CORE-3, V5CORE-4, V5CORE-7, V5CORE-9
                          │
                          │  (epic unblocked)
                          │
  ┌──────────┬───────────┬┴──────────┬──────────┬──────────┬──────────┬──────────┐
  │          │           │           │          │          │          │          │
V5REPOS-2 V5REPOS-3   V5REPOS-4  V5REPOS-5  V5REPOS-6  V5REPOS-7  V5REPOS-8  V5REPOS-12
(ForgePort (repos.    (repos.get) (repos.add) (repos.    (repos.add (repos.    (URL →
 trait)     list)                             remove)   _remote)    remove_    provider
                                                                    _remote)   derivation)
  │                                                                                │
  ├────────────────┬─────────────────┐                                              │
  │                │                 │                                              │
V5REPOS-9      V5REPOS-10        V5REPOS-11                                         │
(GitHub        (Codeberg         (GitLab                                            │
 adapter)       adapter)          adapter)                                          │
  │                │                 │                                              │
  └────────────────┴─────────────────┘                                              │
                   │                                                                │
          (adapters landed — at least one; REPOS-12 also landed for URL routing)    │
                   ├────────────────────────────────────────────────────────────────┘
                   │
         ┌─────────┴─────────┐
         │                   │
     V5REPOS-13          V5REPOS-14
     (repos.sync)        (repos.push)
         │                   │
         └─────────┬─────────┘
                   │
             V5REPOS-15 (REPOS checkpoint)
```

**Phase 1 (8-way parallel):** V5REPOS-2, 3, 4, 5, 6, 7, 8, 12. CRUD tickets
(3–8) touch org YAML storage only. V5REPOS-2 defines the trait. V5REPOS-12
pins the URL-to-provider routing logic as a standalone pure function.

**Phase 2 (3-way parallel, gated on V5REPOS-2):** V5REPOS-9, 10, 11. Each
adapter is independent once the trait is pinned.

**Phase 3 (2-way parallel):** V5REPOS-13, 14. Both depend on V5REPOS-2 + at
least one adapter landed + V5REPOS-12 (for routing). The implementer picks
which adapter to pilot; the others follow via shared interface.

**Phase 4 (checkpoint):** V5REPOS-15.

## Tickets

| ID | Status | Summary |
|----|--------|---------|
| V5REPOS-2  | Pending | `ForgePort` trait — v1 capability: read metadata, write metadata |
| V5REPOS-3  | Pending | `repos.list <org>` — per-org enumeration |
| V5REPOS-4  | Pending | `repos.get <org> <name>` — full detail including remote list |
| V5REPOS-5  | Pending | `repos.add <org> <name>` — writes new repo entry to org yaml |
| V5REPOS-6  | Pending | `repos.remove <org> <name>` — drops repo entry; `delete_remote: bool` defaults false |
| V5REPOS-7  | Pending | `repos.add_remote <org> <name>` — appends a remote; `dry_run` supported |
| V5REPOS-8  | Pending | `repos.remove_remote <org> <name>` — drops a remote by URL |
| V5REPOS-9  | Pending | GitHub `ForgePort` adapter |
| V5REPOS-10 | Pending | Codeberg `ForgePort` adapter (Gitea-compatible) |
| V5REPOS-11 | Pending | GitLab `ForgePort` adapter |
| V5REPOS-12 | Pending | URL → provider derivation — pure function + integration with provider-domain map |
| V5REPOS-13 | Pending | `repos.sync <org> <name>` — pull metadata from remote, diff vs local |
| V5REPOS-14 | Pending | `repos.push <org> <name>` — apply local metadata to remote |
| V5REPOS-15 | Pending | REPOS checkpoint: user-story verification + state map |

## User stories (the checkpoint verifies these)

1. **Register an existing remote.** Given an org with credentials, I can
   add a repo by providing its name and remote URL(s). The URL's domain
   resolves to the correct provider automatically.
2. **Add a mirror.** An existing repo gets a second remote on a different
   provider. No credentials on the second provider are required at add
   time — they're only required at sync/push time.
3. **Remove without destroying.** `repos.remove` by default never deletes
   anything on the forge — it only drops the entry from the org yaml.
   Passing `delete_remote: true` is the only way to trigger a forge-side
   delete.
4. **Sync metadata.** `repos.sync` on a registered repo reports any drift
   between local-yaml and the forge's source of truth (default branch,
   description, archived flag, etc.) without applying changes.
5. **Push metadata.** After editing the org yaml locally, `repos.push`
   brings the forge into agreement.
6. **Cross-provider.** The same repo can sync from its GitHub remote and
   push to its Codeberg remote in the same session — adapter dispatch is
   per-remote, not per-repo.
7. **Custom-domain provider.** A repo whose remote is
   `git@git.example.internal:...` works once the config.yaml provider map
   has that domain, or once the remote has an explicit `provider:` field.

## Contracts pinned here

- **`ForgePort` trait.** Pinned in V5REPOS-2. Consumed by every adapter
  ticket and by V5REPOS-13/14. The trait's capability set is fixed at this
  point in the epic — no adapter ticket may require a method the trait
  doesn't declare; no `sync`/`push` ticket may use a capability not in the
  trait. If a method is missing, add it to the trait in V5REPOS-2 before
  proceeding.
- **Remote shape.** Pinned in V5REPOS-7. A remote is `{url}` string form
  when provider is implied by the domain, or `{url, provider}` object
  form when overridden. Downstream consumers (adapters, sync/push) must
  handle both shapes.
- **Sync result shape.** Pinned in V5REPOS-13. V5WS-9 (workspace sync)
  aggregates these per-member and depends on the shape being stable.

## What must NOT change

- v4's `repo.*` activation. v4 repos operate on a different data model
  (LocalForge `repos.yaml` + per-repo `.hyperforge/config.toml`). v5
  repos read/write the v5 org YAML schema only. Neither touches the
  other's storage.
- The global provider-domain map in `config.yaml` — owned by V5CORE-3.
  V5REPOS-12 *consumes* it but does not redefine its shape.

## Risks

- **R1: GitHub API rate limits during tests.** Integration tests that hit
  the real GitHub API will burn rate limit. Spike: can adapter tests run
  against a local mock (e.g., httpmock) while the checkpoint runs against
  real GitHub once per cycle? Spike ticket is optional — flag for epic
  evaluation to decide.
- **R2: Provider-specific metadata fields.** GitHub has `archived`,
  GitLab has `archived` + `visibility`, Codeberg has its own set.
  V5REPOS-2 must define the trait's metadata shape as the *intersection*
  of what we commit to portably syncing. Non-portable fields are
  per-adapter extensions, not trait members. Spike before locking trait.
- **R3: Two-remote push ordering.** If a repo has a GitHub remote and a
  Codeberg remote and both are out of sync, which does `repos.push`
  update first? V5REPOS-14 must define this — either parallel, or
  ordered by provider-priority in config.yaml, or caller-driven via a
  parameter. Pick one, pin it, don't leave ambiguous.

## Out of scope

- Git transport operations (clone, fetch, push of git refs). v5 Repos
  syncs *metadata only*. Git-level operations are post-v5, likely as a
  separate service (`git.*`) that composes with `repos.*`.
- `repo init` / `.hyperforge/config.toml` management. Out of v5 scope.
- Incremental listing / ETag caching from v4's `list_repos_incremental`.
  v5 adapters may adopt this later; v1 reads fresh.
- Repo rename / move across orgs.
