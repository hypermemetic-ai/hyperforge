# AUTH-3: `auth check` — Token Validation

blocked_by: [AUTH-2]
unlocks: [AUTH-6]

## Scope

A command that validates all configured tokens — checks they exist, aren't expired, and have the right scopes.

## Method

`auth check` on the root HyperforgeService (not a sub-service).

### Params
- `org` — check credentials for a specific org (optional, checks all if omitted)
- `forge` — check a specific forge only (optional)
- `channel` — check credentials for specific dist channels (optional)

### Flow
1. Load all org configs to enumerate org/forge pairs
2. For each, look up required credentials from the registry (AUTH-2)
3. For each credential:
   - Check if it exists in the secrets store
   - If validation method is HttpGet: hit the endpoint, check for 200
   - If GitHubScopes: hit `/user`, parse `X-OAuth-Scopes` response header, check required scopes present
   - Report: valid / missing / invalid (401) / insufficient scopes / expired

### Validation per registry

| Registry | Endpoint | What to check |
|----------|----------|---------------|
| GitHub (token) | `GET /user` | 200 + X-OAuth-Scopes contains `repo` |
| GitHub (packages) | `GET /user` | 200 + X-OAuth-Scopes contains `read:packages` |
| Codeberg | `GET /api/v1/user` | 200 |
| GitLab | `GET /api/v4/user` | 200 |
| crates.io | `GET /api/v1/me` with token header | 200 |
| Hackage | Exists check only (no validation endpoint) | Secret present |

### Events

```rust
AuthCheckResult {
    credential: String,    // display name
    key_path: String,      // secret key
    status: String,        // "valid", "missing", "invalid", "insufficient_scopes", "expired"
    detail: Option<String>, // scope list, error message, etc.
}
```

## Usage

```bash
# Check everything
synapse lforge hyperforge auth check

# Check specific org
synapse lforge hyperforge auth check --org hypermemetic

# Check what's needed for a release
synapse lforge hyperforge auth check --channel forge-release
```

## Acceptance Criteria

- [ ] Detects missing tokens
- [ ] Validates GitHub token scopes via X-OAuth-Scopes header
- [ ] Reports insufficient scopes with clear message about what's missing
- [ ] Works for all supported registries
- [ ] Runs in parallel across orgs/forges
