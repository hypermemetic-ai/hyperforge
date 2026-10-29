---
id: V5CORE-2
title: "Crate scaffold and bare HyperforgeService on port 44105"
status: Complete
type: implementation
blocked_by: []
unlocks: [V5CORE-3, V5CORE-4, V5CORE-5, V5CORE-6, V5CORE-7, V5CORE-8, V5CORE-9]
---

## Problem

No v5 crate exists. Downstream tickets cannot add activations, loaders, or
tests until the crate compiles, depends on plexus 0.5, exposes an empty
`HyperforgeService` root activation, and ships a daemon binary that listens on
a configurable TCP port, defaulting to the v5 port pinned in CONTRACTS D1.

## Required behavior

The crate ships one binary. When started, it registers a Plexus activation
named `lforge-v5` (D1) carrying a `HyperforgeService` with **zero methods and
zero children** and listens for synapse RPC.

Binary CLI inputs:

| Input | Type | Required | Notes |
|---|---|---|---|
| `--port` | `u16` | no | Defaults to `44105` (D1). Any non-default value binds that port instead. |
| `--config-dir` | `FsPath` | no | Defaults to `~/.config/hyperforge/`. Resolved (tilde + env) before use. |

Observable outputs (via synapse against the running daemon):

| Output / Event | Shape | Notes |
|---|---|---|
| Plexus registry entry | activation name `lforge-v5` on the bound port | visible via `synapse -P <port> list` |
| `HyperforgeService` schema | zero methods, zero children | visible via synapse schema introspection |

Edge cases:

- Port already bound: binary exits non-zero with a diagnostic on stderr; no partial registration.
- `--config-dir` points to a non-existent directory: binary creates it (mkdir -p); creation failure is a hard error.
- v4 daemon on `44104` continues to run unaffected.

## What must NOT change

- v4 crate, binaries, port (`44104`), or on-disk state.
- Plexus registry semantics — v5 registers **alongside** v4, never replaces it.

## Acceptance criteria

1. Starting the binary with default flags produces a Plexus registration named `lforge-v5` on port `44105` within 5 seconds.
2. Starting with `--port <N>` binds port `N` instead; registration still uses name `lforge-v5`.
3. `synapse -P <port> list` shows `lforge-v5` among registered activations while the binary runs.
4. `synapse -P <port> lforge hyperforge` introspection returns a schema where `HyperforgeService` declares zero methods and zero children.
5. Running v5 does not alter v4's registry entry on port `44104` (when v4 is running in parallel).
6. Binary exits non-zero when the chosen port is already bound; stderr names the port.

## Completion

- Run `bash tests/v5/V5CORE/V5CORE-2.sh` → exit 0.
- Status flips to `Done` in-commit with the implementation.
