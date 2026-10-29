---
id: V5PARITY-1
title: "v5 → v4 feature parity (driver state)"
status: Epic
type: epic
blocked_by: []
unlocks: []
---

## Goal

Close the remaining v4 → v5 surface gap so v4 can be retired. Out of
the audit on day-of-draft: v5 covers the cleaner data model + orgs/
repos/workspaces CRUD + soft-delete lifecycle + provisioning. It does
NOT cover git transport (clone/fetch/pull/push-refs), import/discover,
analytics, SSH wiring, rename/set-archived/set-default-branch, build,
or the root-level ergonomics (reload, begin, config_*, auth_*).

When this epic is done: **v5 is the daily driver; v4 can be retired.**

## Tickets

| ID | Cluster | Surface |
|----|---------|---------|
| V5PARITY-2  | IMPORT            | `ForgePort::list_repos` + 3 adapters, `repos.import`, `workspaces.discover` |
| V5PARITY-3  | GIT               | `ops::git` (subprocess), `repos.{clone, fetch, pull, push_refs, status, dirty, set_transport}`, workspace-parallel variants |
| V5PARITY-4  | ANALYTICS         | `ops::repo::analytics`, `repos.{size, loc, large_files}`, workspace aggregates |
| V5PARITY-5  | SSH               | `.git/config` `core.sshCommand` wiring, `repos.set_ssh_key`, org-level fallback |
| V5PARITY-6  | LIFECYCLE-EXT     | `repos.{rename, set_default_branch, set_archived}`, `workspaces.{set_default_branch, check_default_branch, verify, check, diff, move_repos}` |
| V5PARITY-7  | AUTH              | `secrets.{set, list_refs, delete}`, `auth_{check, requirements}` |
| V5PARITY-8  | CLI               | `reload`, `config_show`, `config_set_ssh_key`, `config_show_ssh_key`, `begin` |
| V5PARITY-9  | BUILD-MANIFEST    | `build.{unify, analyze, validate, detect_name_mismatches, package_diff}` |
| V5PARITY-10 | BUILD-RELEASE     | `build.{bump, publish, release, release_all}` |
| V5PARITY-11 | BUILD-DIST-EXEC   | `build.{init_configs, binstall_init, brew_formula, dist_init, dist_show, run, exec}` |
| V5PARITY-12 | CLEANUP           | Typed-state tightenings from the data-structure audit + parallel-test harness fix + residuals |
| V5PARITY-13 | Checkpoint        | v4-retireable verification |

## Dependency DAG

```
V5PARITY-2 (IMPORT)
    │
    ├─ V5PARITY-3 (GIT)
    │       │
    │       └─ V5PARITY-5 (SSH)  ─┐
    │                              │
    ├─ V5PARITY-4 (ANALYTICS)     │
    │                              │
    ├─ V5PARITY-6 (LIFECYCLE-EXT) │
    │                              │
    ├─ V5PARITY-7 (AUTH)          │   parallel after their deps
    │                              │
    ├─ V5PARITY-8 (CLI)           │
    │                              │
    ├─ V5PARITY-9 (BUILD-MANIFEST)│
    │       │                      │
    │       ├─ V5PARITY-10 (RELEASE) ─┐
    │       └─ V5PARITY-11 (DIST-EXEC)─┤
    │                                   │
    └─ V5PARITY-12 (CLEANUP)           │
            │                           │
            └──── V5PARITY-13 (CHECKPOINT) — blocks on all above
```

Phase A (foundation): V5PARITY-2.
Phase B (parallelizable): V5PARITY-3, 4, 6, 7, 8.
Phase C (depends on 3): V5PARITY-5.
Phase D (build tree): V5PARITY-9, then 10 + 11 in parallel.
Phase E: V5PARITY-12, then V5PARITY-13.

## What must NOT change

- Every currently-passing v5 test must still pass after each ticket.
- V5LIFECYCLE-11's DRY grep invariants must stay green. Every new method goes through the `ops::` layer; every new adapter method goes on `ForgePort`; no service directly invokes `serde_yaml` / `std::fs` outside the exempted modules.
- v4 code (`src/{adapters,services,service}.rs`, etc.) is reference-only — not modified.
- The tier-2 test config pattern (`HF_V5_TEST_CONFIG_DIR`) is the only auth-for-tests mechanism.

## Risks

- **R1: `git2` vs `Command::new("git")`.** Pinned in V5PARITY-3: shell out to `git`, inherits user env (SSH agent, credential helper, hooks). Decision reason: zero-dep, matches what v4 does, fewer abstraction leaks.
- **R2: Parallel workspace ops + forge rate limits.** Pinned in V5PARITY-3: bounded concurrency (default 4, tunable via param). Ordering: stable alphabetical by `<org>/<name>`.
- **R3: Build integrations require live network (crates.io, Homebrew).** Tier 2 for all V5PARITY-10 / V5PARITY-11 tests, SKIP-clean without env.
- **R4: `workspaces.discover`'s match logic.** Ambiguous when two dirs share an `origin` — reuse D5 "first scanned wins alphabetically + emit ambiguous event".
- **R5: V5PARITY-5 SSH wiring writes to `.git/config` directly.** Respect existing `[remote ...]` entries — non-destructive; add `[includeIf]` if already present? Pin during V5PARITY-5 drafting; not locked in the epic.

## Out of scope (explicit)

- Plexus-macros request-based auth forwarding (V5AUTH path B from the earlier discussion) — separate epic if adopted.
- Migrating v4 users' existing repos.yaml + .hyperforge/config.toml to v5's shape — migration tool is a post-parity epic.
- MCP HTTP server mode (`--mcp` flag on v4 daemon) — not a daily-driver capability.
