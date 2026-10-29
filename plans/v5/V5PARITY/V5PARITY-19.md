---
id: V5PARITY-19
title: "WS-FILTER-DRYRUN — glob filter + --dry_run on every workspaces.* method"
status: Pending
type: implementation
blocked_by: [V5PARITY-14]
unlocks: []
---

## Problem

Workspace operations are all-or-nothing today — `workspaces.tag --tag v0.1.0` tags every member. That's the right default but the wrong only-option:

1. **Selective ops.** "Tag only the `*-cli` members" or "fetch only `core/*`" needs a filter. Today you'd drop to per-repo calls and lose the aggregate summary.
2. **Preview before apply.** Mutating ops (clone/checkout/commit/tag/push/pull) commit changes immediately. There's no "show me what would happen" mode beyond `dry_run` on a few specific methods (`workspaces.discover`).

## Required behavior

**`--filter <glob>` on every `workspaces.*` method.** Pattern matches against the member's `<org>/<name>` (workspace shorthand form). Standard shell-glob syntax (`*`, `?`, `[...]`, `**` for cross-segment). Multiple patterns via comma-separated list. Members that don't match are silently skipped (no event emitted, not counted toward `total`).

```
workspaces.tag --name W --tag v1 --filter "demo/*-cli"
workspaces.fetch --name W --filter "{core/*,libs/util}"
```

**`--dry_run true` on every mutating method.** Read-only methods (`status`, `diff`, `discover`) ignore the flag. Mutating methods (`clone`, `fetch`, `pull`, `push`, `checkout`, `commit`, `tag`, `sync`) emit the events they would have emitted, marked with `dry_run: true`, but skip the actual `ops::git::*` call. No filesystem or network side effect.

**Aggregate semantics with filter:**
- `total` counts only members that matched the filter (not the workspace's full member count).
- An additional summary field `filtered_out: u32` reports how many members were excluded by the filter.

**`dry_run: true` events** carry a discriminator (`"dry_run": true` field on every per-member event when in that mode). The aggregate's `ok` counter still reports successes-that-would-have-happened; `errored` counts pre-flight failures (member dir missing, etc.) that the real run would have hit.

## What must NOT change

- Public method names — only new optional params get added.
- Filter syntax is glob (not regex). Keep it predictable from the shell.
- D6 partial-failure tolerance — filter doesn't change error handling, only which members participate.
- The non-`workspaces.*` services (`repos.*`, `orgs.*`) are out of scope for this ticket.

## Acceptance criteria

1. `workspaces.fetch --name W --filter "demo/alpha"` against a 3-member workspace emits exactly one `member_git_result` and a summary with `total: 1, filtered_out: 2`.
2. `workspaces.tag --name W --tag v1 --filter "demo/{alpha,beta}"` matches both alpha and beta.
3. `workspaces.tag --name W --tag v1 --dry_run true` emits two `member_git_result { status: "ok", dry_run: true }` events and applies no actual tag (verified via `git tag --list` on each member returning empty).
4. `workspaces.status --name W --dry_run true` works identically to without the flag (read-only methods ignore the param).
5. Invalid glob (e.g., unclosed bracket) emits a `validation` error before iteration starts.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-19.sh` → exit 0 (tier 1).
- Ready → Complete in-commit.
