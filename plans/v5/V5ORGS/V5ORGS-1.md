---
id: V5ORGS-1
title: "Hyperforge v5 Orgs — CRUD + Credentials"
status: Epic
type: epic
blocked_by: [V5CORE-3, V5CORE-4, V5CORE-6, V5CORE-9]
unlocks: []
---

## Goal

Populate the `OrgsService` stub (registered in V5CORE-6) with the full set of
org CRUD + credential-management methods. At completion, an org can be
created, inspected, updated, and deleted entirely through synapse RPC — no
hand-editing of `~/.config/hyperforge/orgs/<org>.yaml` is ever required,
and credentials can be added/removed on an existing org without rewriting
the org file wholesale.

When this epic is done:

- `hyperforge orgs list` enumerates every org known to the daemon.
- `hyperforge orgs get <name>` returns one org's full detail, including
  forge provider, credentials (by key + type, not value), and its repo set.
- `hyperforge orgs create <name> --provider <p>` writes a new org file and
  the daemon picks it up without a restart.
- `hyperforge orgs delete <name>` removes the org file (with a `dry_run`
  mode to preview).
- `hyperforge orgs update <name> …` patches an existing org file
  (provider change, rename, etc.) without clobbering unmentioned fields.
- `hyperforge orgs set_credential <name> --key <k> --type <t> …` and
  `hyperforge orgs remove_credential <name> --key <k>` mutate only the
  credentials list, preserving all other fields.
- No method ever writes a secret *value* to `orgs/<name>.yaml` — only
  `secrets://…` references or filesystem paths (for SSH keys).

## Dependency DAG

```
                    V5CORE-3, V5CORE-4, V5CORE-6, V5CORE-9
                                  │
                                  │  (epic unblocked)
                                  │
      ┌──────────┬──────────┬─────┼──────┬───────────┬───────────┐
      │          │          │    │       │           │           │
  V5ORGS-2  V5ORGS-3   V5ORGS-4  V5ORGS-5  V5ORGS-6  V5ORGS-7  V5ORGS-8
  (orgs.    (orgs.get) (orgs.    (orgs.    (orgs.    (orgs.set_ (orgs.remove_
   list)               create)   delete)   update)   credential) credential)
      │          │          │       │         │          │           │
      └──────────┴──────────┴───────┼─────────┴──────────┴───────────┘
                                    │
                              V5ORGS-9 (ORGS checkpoint)
```

**Phase 1 (7-way parallel):** V5ORGS-2 through V5ORGS-8. All seven touch the
same storage primitive (org YAML loader from V5CORE-3) but different
methods. No read-after-write dependency exists *within the epic* — each
ticket owns its own method and its own bash-level test against a fresh
fixture.

**Phase 2 (checkpoint):** V5ORGS-9 verifies the composed surface against
the user stories that motivated the epic.

## Tickets

| ID | Status | Summary |
|----|--------|---------|
| V5ORGS-2 | Pending | `orgs.list` — returns summary per org |
| V5ORGS-3 | Pending | `orgs.get` — returns full detail (including credential refs, not values) |
| V5ORGS-4 | Pending | `orgs.create` — writes new org file with validation + `dry_run` |
| V5ORGS-5 | Pending | `orgs.delete` — removes org file with `dry_run` |
| V5ORGS-6 | Pending | `orgs.update` — patches provider / metadata fields |
| V5ORGS-7 | Pending | `orgs.set_credential` — adds or updates one credential entry by key |
| V5ORGS-8 | Pending | `orgs.remove_credential` — removes one credential entry by key |
| V5ORGS-9 | Pending | ORGS checkpoint: user-story verification + state map |

## User stories (the checkpoint verifies these)

1. **Onboard a new org.** As a user with zero orgs configured, I can
   create `hypermemetic` with a github provider and one token credential
   entirely via synapse commands.
2. **Inspect without leaking.** `orgs get hypermemetic` reveals the
   credential's key and type but never its resolved value.
3. **Rotate a credential.** I can replace the value-reference of an
   existing credential without rewriting any other org field.
4. **Delete with confidence.** `orgs delete hypermemetic --dry_run true`
   tells me what would change. A real `orgs delete` removes only that
   org's file, leaving every other org and every workspace yaml intact.
5. **Survive restart.** After any of the above, restarting the daemon
   yields the same `orgs list` output — state lives on disk, not in memory.

## Contracts pinned here

- **Org summary shape.** Pinned in V5ORGS-2. Used by V5WS-9 when listing
  workspace members by org.
- **Credential reference shape.** Pinned in V5ORGS-7. V5REPOS adapters
  consume this reference to obtain the resolved secret via the V5CORE-4
  resolver trait.

## What must NOT change

- v4's `orgs_add` / `orgs_delete` / `orgs_list` behavior. v5's methods
  live under `orgs.*` (distinct namespace). Both must coexist.
- The shape of any file under `~/.config/hyperforge/orgs/` that v4 owns.
  v5 reads and writes `<org>.yaml` (v5's schema). If v4 ever wrote a
  different filename or extension to the same directory, v5 leaves it
  untouched.

## Risks

- **R1: Credential-value leakage.** `orgs.get` must never return a
  resolved secret value. Acceptance criteria in V5ORGS-3 must assert
  absence, not just presence of the reference.
- **R2: Concurrent writes.** Two `orgs.set_credential` calls on the same
  org could race. The storage primitive from V5CORE-3 must define the
  concurrency contract (atomic replace or sequential lock). Tickets
  V5ORGS-6/7/8 inherit it — they don't redefine it.

## Out of scope

- Org rename. (Future epic — renaming an org means cascading updates to
  every workspace YAML that references it.)
- Credential rotation workflows (e.g., "rotate all GitHub tokens across
  all orgs"). Single-credential rotation is in scope; batch is not.
- Listing orgs scoped by provider. (Client-side filter if needed.)
