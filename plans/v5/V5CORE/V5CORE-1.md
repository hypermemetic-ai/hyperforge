---
id: V5CORE-1
title: "Hyperforge v5 Core — Scaffolding, Config, Harness"
status: Epic
type: epic
blocked_by: []
unlocks: [V5ORGS-1, V5REPOS-1, V5WS-1]
---

## Goal

Stand up the v5 activation tree's foundation: a fresh crate wired against
`plexus-core = "0.5"` / `plexus-macros = "0.5"`, a `HyperforgeService` root
with a working `status` method and three static child service *stubs* (empty
activations reserved for ORGS/REPOS/WS epics to flesh out), a config file
subsystem that loads and round-trips the three YAML schemas (`config.yaml`,
`orgs/<org>.yaml`, `workspaces/<ws>.yaml`), an embedded secret store that
resolves `secrets://…` references, and a shared test harness that spawns
the daemon on an ephemeral port and executes synapse bash assertions
against it.

When this epic is done:

- `synapse -P <port> lforge hyperforge status` returns `{version, config_dir}`.
- Hand-written fixtures for all three YAML schemas load without error and
  round-trip losslessly.
- A `secrets://<path>` reference in an org file resolves to the value
  stored under that path in `secrets.yaml`.
- Three empty child activations (`orgs`, `repos`, `workspaces`) appear in
  the full schema exported by the daemon — each exposing zero methods but
  reserving the namespace.
- Any downstream epic can `cargo test` using the shared harness without
  re-implementing daemon lifecycle.
- v5 coexists with v4 on disk — no files from v4 are moved, renamed, or
  deleted.

## Dependency DAG

```
                V5CORE-2  (crate scaffold + plexus 0.5 + empty HyperforgeService)
                      │
   ┌────────┬─────────┼─────────┬─────────┬─────────┬─────────┬─────────┐
   │        │         │         │         │         │         │         │
 V5CORE-3 V5CORE-4  V5CORE-5  V5CORE-6  V5CORE-7  V5CORE-8  V5CORE-9   │
 (config  (secret   (status   (OrgsService  (ReposService (Workspcs (integ.    │
  yaml     store,    method)   stub)     stub)    Service stub) test       │
  loaders) secrets://                                        harness)   │
                                                                        │
   └─────────────────┬──────────────────────────────────────────────────┘
                     │
                 V5CORE-10 (CORE checkpoint — state-of-epic)
```

**Phase 1 (sequential):** V5CORE-2. Nothing else compiles until the crate exists.

**Phase 2 (7-way parallel):** V5CORE-3, 4, 5, 6, 7, 8, 9 all touch disjoint
surfaces. None of them read each other's outputs. Each may be implemented
by a separate worker concurrently.

**Phase 3 (checkpoint):** V5CORE-10 verifies the composed surface.

## Tickets

| ID | Status | Summary |
|----|--------|---------|
| V5CORE-2  | Pending | Crate scaffold: workspace member, plexus 0.5 deps, bare HyperforgeService, daemon binary listens on `--port` |
| V5CORE-3  | Pending | YAML config loaders: global, org, workspace — schemas + round-trip |
| V5CORE-4  | Pending | Embedded secret store: YAML backend, `secrets://<path>` resolver trait |
| V5CORE-5  | Pending | `HyperforgeService::status` — returns version + config_dir, queryable via synapse |
| V5CORE-6  | Pending | `OrgsService` static child stub — empty activation registered on root |
| V5CORE-7  | Pending | `ReposService` static child stub — empty activation registered on root |
| V5CORE-8  | Pending | `WorkspacesService` static child stub — empty activation registered on root |
| V5CORE-9  | Pending | Integration test harness: ephemeral-port daemon fixture, synapse bash runner callable from Rust `#[test]` |
| V5CORE-10 | Pending | CORE checkpoint: verify scaffolding composes (all stubs discoverable, status works, fixtures round-trip, harness self-tests) |

## Why stubs are separate tickets

Splitting the three service stubs (V5CORE-6/7/8) into individual tickets gives
the downstream epics (ORGS, REPOS, WS) a clean starting line — each can
begin as soon as its corresponding stub lands, without waiting for the other
two. It also means the "an empty child activation appears in the schema"
contract is pinned per-service, so when V5ORGS adds methods to `orgs`, the
schema-shape contract doesn't drift.

## Test strategy

Every V5CORE ticket's acceptance criteria are verified by a bash script
that invokes synapse against a daemon spawned by the harness (V5CORE-9).
V5CORE-9 itself is the exception — its acceptance criteria verify the
harness *can be invoked* by a Rust `#[test]` and that the harness detects
both daemon-up and daemon-down states correctly.

Fixtures live under `tests/fixtures/v5core/` in the crate. Each ticket that
introduces a fixture commits it as part of its completion.

## Contracts pinned here (downstream-facing)

- **Config directory shape.** `~/.config/hyperforge/` with `config.yaml`,
  `orgs/<org>.yaml`, `workspaces/<ws>.yaml`, `secrets.yaml`. Pinned in
  V5CORE-3. Every V5ORGS / V5WS ticket reads from this layout.
- **Secret resolver trait.** A trait with `resolve(&str) -> Option<String>`
  (name is illustrative — the implementer picks). Pinned in V5CORE-4. Every
  V5REPOS adapter ticket consumes this trait to obtain credentials.
- **Status event shape.** Pinned in V5CORE-5. V5CORE-10 checkpoint asserts
  it, and future health-check tooling depends on it.
- **Harness API surface.** Pinned in V5CORE-9. Every downstream ticket's
  integration test invokes the harness by this surface.

## What must NOT change

- v4 binaries, config, or on-disk state. v5 writes to the same
  `~/.config/hyperforge/` tree but only adds files — never mutates v4 ones.
  v4's port (44104) remains v4's. v5 uses a different default port; the
  choice is pinned in V5CORE-2.

## Out of scope

- Any method on `OrgsService`, `ReposService`, `WorkspacesService` beyond the empty
  stub — those are owned by the downstream epics.
- Docs (`README.md` / skill updates) — each method-level ticket owns its
  own docs. CORE tickets produce the crate-level skeleton only.
- v4 retirement. That's a post-v5 cleanup epic.
