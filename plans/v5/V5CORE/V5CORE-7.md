---
id: V5CORE-7
title: "ReposService static child stub"
status: Complete
type: implementation
blocked_by: [V5CORE-2]
unlocks: [V5CORE-10, V5REPOS-1]
---

## Problem

The V5REPOS epic will attach methods to a `repos` namespace on the root
activation. That namespace must already exist as a zero-method static
child before V5REPOS-1 promotes.

## Required behavior

Register **one** static child on `HyperforgeService` at path segment `repos`,
pointing to a new activation named `ReposService`. `ReposService` declares:

- zero methods
- zero children

Static declaration only — no dynamic children in v1 (README §3).

Edge cases:

- Introspecting `repos` returns an empty method list and empty child list.
- Invoking any method name under `repos` returns synapse's standard "method not found" error; defining that error shape is not this ticket's job.

## What must NOT change

- `HyperforgeService`'s existing method set.
- The `orgs` and `workspaces` child namespaces (owned by V5CORE-6 and V5CORE-8).
- v4's `RepoService` on the v4 crate/port.

## Acceptance criteria

1. Schema introspection shows a child of `HyperforgeService` at path segment `repos`.
2. The `repos` node's method list is empty.
3. The `repos` node's child list is empty.
4. The activation backing `repos` is named `ReposService` in the schema.
5. Invoking `hf_cmd repos does_not_exist` emits an error event; no panic or daemon crash.

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-7.sh` → exit 0.
- Status flips in-commit with the implementation.
