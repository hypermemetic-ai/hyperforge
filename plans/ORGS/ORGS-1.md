---
id: ORGS-1
title: "Org CRUD via RPC — Overview"
status: Epic
type: epic
blocked_by: []
unlocks: []
---

## Goal

Hyperforge today exposes `orgs_list` and `orgs_delete` on the root service,
but has no programmatic way to *create* an org. Onboarding a new org
(e.g. `juggernautlabs`) requires hand-editing
`~/.config/hyperforge/orgs/<org>.toml` and calling `reload`. The hand-edit
is the single friction point that breaks the otherwise-complete
RPC-driven UX.

When this epic is done:

- `orgs_add` creates a new org config file from RPC parameters, with
  validation and a dry-run mode (mirroring `orgs_delete`'s shape).
- `orgs_update` mutates an existing org's ssh map and/or workspace_path
  without hand-editing.
- Each method-level ticket also updates the surfaces that document it
  (README, skill doc) as part of its completion — docs are owned by the
  ticket that ships the behavior, not a separate doc-sweep ticket.
- A final ticket (ORGS-4) verifies that the shipped methods *compose*
  into the user stories the epic was motivated by — automated E2E tests
  that run the real Synapse → Plexus → service → filesystem round-trip.
- Existing behavior (`orgs_list`, `orgs_delete`) is unchanged.

## Dependency DAG

```
        ┌── ORGS-2  orgs_add RPC    ─┐
(root) ─┤     (+ docs for orgs_add)  ├── ORGS-4  E2E user-story tests
        └── ORGS-3  orgs_update RPC ─┘
              (+ docs for orgs_update)
```

ORGS-2 and ORGS-3 are parallel — they touch different methods, share no
state, and each owns the documentation for its own surface (README.md
cheatsheet row, skill doc cheatsheet row, AUTH_IMPLEMENTATION.md
retrospective note referring to the new method).

ORGS-4 is the epic's "final ticket" — its purpose is not new behavior
but verifying that the earlier tickets compose into the agreed user
stories. See the planning skill's "Final ticket — end-to-end
verification" section for the pattern.

## Tickets

| ID | Status | Summary |
|----|--------|---------|
| ORGS-2 | Pending | `orgs_add` RPC with dry-run + validation (+ its own docs) |
| ORGS-3 | Pending | `orgs_update` RPC for ssh and workspace_path (+ its own docs) |
| ORGS-4 | Pending | E2E integration tests anchored on user stories |

## Out of scope

- Token management (lives in `secrets.yaml` via the auth sidecar; a
  separate concern).
- Per-forge `owner_type: user|org` (not a field on `OrgConfig` today;
  handling it is a separate epic if/when the adapter surface requires it).
- Renaming orgs. `orgs_delete` + `orgs_add` composes as a workaround.
- Bulk import. `repo import` already covers the repo-level case; adding
  a multi-org bulk command can be layered later.

## Implementation notes (shared context)

- `OrgConfig` is at `src/config/org.rs:12-21`. Fields: `ssh:
  HashMap<String, String>`, `workspace_path: Option<String>`.
- `OrgConfig::save()` (`src/config/org.rs:39-50`) writes the TOML
  idempotently. No new write machinery required.
- Root service is at `src/service.rs`; activation macro at lines 476-481.
  Existing org methods: `orgs_list` (905-960), `orgs_delete` (967-1049).
- Macros use the current `activation` / `method` names (not the
  deprecated `activation` / `method`).
- Integration test pattern lives in `tests/integration_test.rs` —
  construct `HyperforgeService`, register in `DynamicService`, route
  `"hyperforge.<method>"`, match on `HyperforgeEvent` variants.
