# AUTH-4: `auth setup` — Guided Credential Setup

blocked_by: [AUTH-2]
unlocks: []

## Scope

A command that walks the user through obtaining and configuring the right tokens for an org/forge/channel combination. Shows exactly what to create, where, and with which scopes.

## Method

`auth setup` on the root HyperforgeService.

### Params
- `org` — organization name (required)
- `forge` — forge to set up (optional, sets up all configured forges if omitted)
- `channel` — distribution channel to set up (optional)

### Flow
1. Determine which credentials are needed (from credential registry)
2. Check which are already configured and valid (via auth check logic)
3. For each missing/invalid credential:
   - Emit the setup URL
   - Emit instructions (which scopes, classic vs fine-grained, etc.)
   - Emit the key path where the token should be stored
   - Provide the synapse command to store it:
     ```
     synapse -P 44105 secrets auth set_secret --secret_key "github/hypermemetic/packages_token" --value "$(pbpaste)"
     ```
4. After user stores each token, validate it

### Output example

```
Setting up credentials for hypermemetic on github...

✗ github/hypermemetic/packages_token — MISSING
  Needed for: ghcr (container images), package listing
  Type: Classic Personal Access Token (NOT fine-grained)
  Required scopes: read:packages, write:packages
  Create at: https://github.com/settings/tokens/new?scopes=read:packages,write:packages
  Then run:
    synapse -P 44105 secrets auth set_secret --secret_key "github/hypermemetic/packages_token" --value "$(pbpaste)"

✓ github/hypermemetic/token — valid (scopes: repo, read:org)

✗ crates-io/token — MISSING
  Needed for: crates-io (cargo publish)
  Create at: https://crates.io/me
  Then run:
    synapse -P 44105 secrets auth set_secret --secret_key "crates-io/token" --value "$(pbpaste)"
```

## Acceptance Criteria

- [ ] Shows clear instructions per credential type
- [ ] Provides direct URLs to token creation pages
- [ ] Provides copy-pasteable synapse commands to store tokens
- [ ] Distinguishes classic vs fine-grained GitHub PATs
- [ ] Skips already-valid credentials
- [ ] Works per-org, per-forge, or per-channel
