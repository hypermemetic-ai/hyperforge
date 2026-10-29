---
id: V5CORE-6
title: "OrgsService static child stub"
status: Complete
type: implementation
blocked_by: [V5CORE-2]
unlocks: [V5CORE-10, V5ORGS-1]
---

## Problem

The V5ORGS epic will attach methods to an `orgs` namespace on the root
activation. That namespace must already be reserved — present as a static
child with zero methods — before V5ORGS-1 promotes. Attaching to a
not-yet-existent namespace would force V5ORGS to edit V5CORE files.

## Required behavior

Register **one** static child on `HyperforgeService` at the path segment
`orgs`, pointing to a new activation named `OrgsService`. `OrgsService` declares:

- zero methods
- zero children

The child is statically declared — no runtime registration, no
`list_children` / `search_children` (README §3 invariant).

Edge cases:

- Introspecting the `orgs` schema returns an empty method list and empty child list — not an error.
- Invoking any method name under `orgs` returns a standard "method not found" error from the synapse layer; this ticket does not define that error shape.

## What must NOT change

- The root `HyperforgeService` method set (still exactly `status` after V5CORE-5 lands).
- The other two service namespaces — `repos` and `workspaces` — are owned by V5CORE-7 and V5CORE-8 respectively.
- v4's `OrgsService` (different crate, different port) remains untouched.

## Acceptance criteria

1. Schema introspection of the v5 daemon shows a child of `HyperforgeService` at path segment `orgs`.
2. The `orgs` node's method list is empty.
3. The `orgs` node's child list is empty.
4. The activation backing `orgs` is named `OrgsService` in the schema.
5. Invoking `hf_cmd orgs does_not_exist` emits an error event; no panic or daemon crash.

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-6.sh` → exit 0.
- Status flips in-commit with the implementation.
