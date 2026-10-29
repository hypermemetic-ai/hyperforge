# LocalForge Stale Repo Discovery

Date: 2026-02-19

## Confirmed Renames (old repo deleted on both remotes)

| Old Name | New Name | Rename Date | Evidence | Old Repo Exists? |
|---|---|---|---|---|
| (the macro crate's former name) | `plexus-macros` | Feb 4, 2026 | Commit `1fb6780`: "refactor: rename package … -> plexus-macros" | No (GitHub 404, no Codeberg repo) |
| (the core crate's former name) | `plexus-core` | Feb 4, 2026 | Commits `ab0e0ed`, `b62a69c`: rename + lib name fix | No (GitHub 404, no Codeberg repo) |
| `substrate-protocol` | `plexus-protocol` | Feb 4, 2026 | Commit `d5b5a81`: cabal file + module rename (Substrate.* -> Plexus.*) | No (GitHub 404, no Codeberg repo) |
| `substrate-sandbox-ts` | `plexus-sandbox-ts` | Feb 4, 2026 | Commit `3c4c7fe`: terminology update. Note: `package.json` name still says `substrate-sandbox-ts` | No (GitHub 404, no Codeberg repo) |

All four renames happened on the same day (Feb 4, 2026) as part of the rebrand to plexus.

## Ghost Repos (no local dir, no remote — pure LocalForge cruft)

| Name | Description | Present On (LocalForge claims) | GitHub? | Codeberg? | Local Dir? |
|---|---|---|---|---|---|
| `lforge-demo` | "LFORGE2 demonstration repository" | github | 404 | N/A | No |
| `synapse-cli-from-ir` | null | github | 404 | N/A | No |
| `hypermemetic-infra` | "Personal infrastructure for multi-forge SSH key and repository management" | codeberg | 404 | Unchecked | No |
| `hypermemetic` | "Hypermemetic infrastructure - multi-forge management..." | github | 404 | N/A | No |
| `plexus-axon` | null | github | 404 | N/A | No |
| `dockerfiles` | "Docker configuration files and base images" | github | 404 | N/A | No |

All 6 have `managed: false` in LocalForge. All appear to be abandoned experiments or consolidated projects.

## Legitimate Missing Repo

| Name | Status | Action Needed |
|---|---|---|
| `service-codegen` | Exists on GitHub, missing on Codeberg | Create on Codeberg via sync |

## Recommended Actions

### 1. Delete 10 stale entries from LocalForge

These 10 repos should be removed from `~/.config/hyperforge/orgs/hypermemetic/repos.yaml`:

**Renamed (old names):**
- the macro crate (now `plexus-macros`)
- the core crate (now `plexus-core`)
- `substrate-protocol` (now `plexus-protocol`)
- `substrate-sandbox-ts` (now `plexus-sandbox-ts`)

**Ghosts (deleted/never existed):**
- `lforge-demo`
- `synapse-cli-from-ir`
- `hypermemetic-infra`
- `hypermemetic`
- `plexus-axon`
- `dockerfiles`

### 2. Create service-codegen on Codeberg

Run sync or manually create the repo.

### 3. Fix plexus-sandbox-ts package.json

`package.json` still has `"name": "substrate-sandbox-ts"` — should be updated to `plexus-sandbox-ts`.

### 4. Fix plexus-core GitHub description

GitHub description still says the core crate's former name — should be updated to match the new name.
