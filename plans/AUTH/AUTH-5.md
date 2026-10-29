# AUTH-5: `auth requirements` — Derive Needed Credentials from Workspace

blocked_by: [AUTH-2]
unlocks: [AUTH-6]

## Scope

Given a workspace with dist configs (DIST-10), enumerate every credential that would be needed to perform a full release — and report which are present vs missing.

## Method

`auth requirements` on the root HyperforgeService.

### Params
- `path` — workspace path (required)
- `include` / `exclude` — repo filters (optional)

### Flow
1. Discover workspace
2. For each repo, load dist config from `.hyperforge/config.toml`
3. Collect the union of all channels, forges, and orgs
4. Map to required credentials via the credential registry (AUTH-2)
5. Deduplicate (many repos share the same org/forge tokens)
6. Check which exist in the secrets store
7. Report: needed / present / missing

### Output example

```
Scanning workspace at /Users/shmendez/dev/controlflow/hypermemetic...

Credentials needed for 32 repos across 1 org, 2 forges, 4 channels:

  ✓ github/hypermemetic/token — present
  ✗ github/hypermemetic/packages_token — MISSING (needed by: ghcr)
  ✓ codeberg/hypermemetic/token — present
  ✗ crates-io/token — MISSING (needed by: 15 repos with crates-io channel)
  ✗ hackage/username — MISSING (needed by: 3 repos with hackage channel)
  ✗ hackage/password — MISSING (needed by: 3 repos with hackage channel)

3 of 6 credentials configured. Run `auth setup --org hypermemetic` to set up missing ones.
```

## Acceptance Criteria

- [ ] Scans workspace dist configs
- [ ] Deduplicates credentials across repos
- [ ] Reports which repos need each credential
- [ ] Shows count of configured vs missing
- [ ] Suggests `auth setup` command for missing credentials
