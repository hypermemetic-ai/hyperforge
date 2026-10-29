---
id: V5CORE-8
title: "WorkspacesService static child stub"
status: Complete
type: implementation
blocked_by: [V5CORE-2]
unlocks: [V5CORE-10, V5WS-1]
---

## Problem

The V5WS epic will attach methods to a `workspaces` namespace on the root
activation. That namespace must already exist as a zero-method static
child before V5WS-1 promotes.

## Required behavior

Register **one** static child on `HyperforgeService` at path segment
`workspaces`, pointing to a new activation named `WorkspacesService`.
`WorkspacesService` declares:

- zero methods
- zero children

Static declaration only (README §3).

Edge cases:

- Introspecting `workspaces` returns an empty method list and empty child list.
- Invoking any method name under `workspaces` returns synapse's standard "method not found" error; defining that error shape is not this ticket's job.

## What must NOT change

- `HyperforgeService`'s existing method set.
- The `orgs` and `repos` child namespaces (owned by V5CORE-6 and V5CORE-7).
- v4's `WorkspaceService` on the v4 crate/port.

## Acceptance criteria

1. Schema introspection shows a child of `HyperforgeService` at path segment `workspaces`.
2. The `workspaces` node's method list is empty.
3. The `workspaces` node's child list is empty.
4. The activation backing `workspaces` is named `WorkspacesService` in the schema.
5. Invoking `hf_cmd workspaces does_not_exist` emits an error event; no panic or daemon crash.

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-8.sh` → exit 0.
- Status flips in-commit with the implementation.
