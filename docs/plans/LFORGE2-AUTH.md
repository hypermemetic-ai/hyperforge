# LFORGE2-AUTH: Authentication Architecture

**Status**: Planning
**Epic**: Auth Infrastructure
**Goal**: Define auth abstraction and integrate with hyperforge

---

## Overview

Authentication is handled as a **remote service** via the **auth-service plugin**. All plugins (including hyperforge) interface with auth through a common abstraction. This allows the backend implementation (WorkOS, local keychain, etc.) to be swapped without changing plugin code.

**Key principle**: Always request tokens under **org scope** - `{forge|registry}/{org}/*`

---

## Auth Abstraction Interface

### The Auth Plugin Protocol

Every service system component uses this interface to request secrets:

```rust
#[async_trait]
pub trait AuthClient: Send + Sync {
    /// Get a secret by path, automatically requesting scope if needed
    async fn get_secret(&self, path: &SecretPath) -> Result<Secret>;

    /// Store a secret (if provider supports writes)
    async fn put_secret(&self, path: &SecretPath, value: &str) -> Result<()>;

    /// List available secrets under prefix (for debugging/discovery)
    async fn list_secrets(&self, prefix: &str) -> Result<Vec<SecretPath>>;

    /// Check if we have access to a scope (non-blocking)
    async fn has_scope(&self, scope: &SecretScope) -> bool;
}

/// Hierarchical secret path: forge/org/token or registry/org/token
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretPath {
    segments: Vec<String>,
}

impl SecretPath {
    /// Parse from string: "github/alice/token"
    pub fn parse(path: &str) -> Result<Self>;

    /// Build from components
    pub fn new(forge: &str, org: &str, key: &str) -> Self;

    /// Get parent scope: "github/alice/token" → "github/alice/*"
    pub fn parent_scope(&self) -> SecretScope;

    /// Render as string
    pub fn as_str(&self) -> &str;
}

/// Scope represents permission to access secrets under a prefix
#[derive(Debug, Clone)]
pub struct SecretScope {
    pub prefix: String,  // "github/alice/*"
    pub permissions: Permissions,
}

#[derive(Debug, Clone)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
}

/// A secret value with metadata
#[derive(Debug, Clone)]
pub struct Secret {
    pub value: String,
    pub path: SecretPath,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}
```

### How Auth Client Works (Black Box)

```rust
impl AuthClient {
    async fn get_secret(&self, path: &SecretPath) -> Result<Secret> {
        // 1. Check local cache for access token with this scope
        if let Some(cached) = self.check_cache(path) {
            return Ok(cached);
        }

        // 2. Determine required scope from path
        let scope = path.parent_scope();  // "github/alice/*"

        // 3. Request scope from auth-service
        // [BLACK BOX: How scope is requested/granted is implementation detail]
        let access_token = self.request_scope(scope).await?;

        // 4. Fetch secret from storage using access token
        // [BLACK BOX: Where secrets are stored is implementation detail]
        let secret = self.fetch_from_storage(&access_token, path).await?;

        // 5. Cache for future requests
        self.cache_secret(path, &secret);

        Ok(secret)
    }
}
```

**What's abstracted away (implementation holes):**
- How scopes are requested (RPC? gRPC? HTTP?)
- How scopes are granted (auto? user approval? timeout?)
- Where secrets are stored (WorkOS Vault? Keychain? File?)
- How access tokens work (JWT? opaque tokens?)
- How master tokens are managed (login flow? refresh?)

---

## Hyperforge's Auth Integration

### Auth Dependencies

Hyperforge needs secrets for:

1. **Forge operations** (push, create repo):
   - `github/{org}/token`
   - `codeberg/{org}/token`
   - `gitlab/{org}/token`

2. **Registry operations** (publish packages):
   - `cargo/token` (user-global)
   - `npm/{org}/token` (org-scoped for npm orgs)
   - `npm/token` (user-global npm)
   - `pypi/token` (user-global)
   - `hex/token` (user-global)
   - `hackage/token` (user-global)

### How Hyperforge Determines Org

**Ground truth**: `.hyperforge/config.toml`

```toml
# Explicit org configuration
org = "alice"  # Used for all forges unless overridden

forges = ["github", "codeberg"]

# Per-forge org override
[forges.github]
org = "acme-corp"  # Use acme-corp for GitHub

[forges.codeberg]
org = "alice"  # Use alice for Codeberg
```

**Fallback**: Extract from git remote URL

```bash
# Remote: git@github.com:alice/my-repo.git
# Extract org: alice
# Use: github/alice/token
```

**Never**: Use SSH host name (too unreliable)

### Auth Flow Examples

#### Example 1: Init (No auth needed)

```bash
hyperforge init --path . --forges github,codeberg --org alice
```

**What happens:**
1. Create `.hyperforge/config.toml`:
   ```toml
   org = "alice"
   forges = ["github", "codeberg"]
   ```
2. Sync git remotes (no API calls yet):
   ```
   origin: git@github.com:alice/repo-name.git
   codeberg: git@codeberg.org:alice/repo-name.git
   ```
3. **No auth requests** - just configuration

**Secrets requested:** None

---

#### Example 2: Push (Git operations)

```bash
hyperforge push --path .
```

**What happens:**
1. Read `.hyperforge/config.toml`:
   ```toml
   org = "alice"
   forges = ["github", "codeberg"]
   ```

2. For each forge, build secret path:
   ```rust
   let github_path = SecretPath::new("github", "alice", "token");
   let codeberg_path = SecretPath::new("codeberg", "alice", "token");
   ```

3. Request secrets (lazy, on-demand):
   ```rust
   let github_token = auth.get_secret(&github_path).await?;
   // First request triggers scope request: "github/alice/*"

   let codeberg_token = auth.get_secret(&codeberg_path).await?;
   // Triggers: "codeberg/alice/*"
   ```

4. Push with tokens:
   ```rust
   git.push_with_token("origin", &github_token)?;
   git.push_with_token("codeberg", &codeberg_token)?;
   ```

**Secrets requested:**
- `github/alice/token` (scope: `github/alice/*`)
- `codeberg/alice/token` (scope: `codeberg/alice/*`)

---

#### Example 3: Create repo on forge (Forge API)

```bash
hyperforge init --path . --forges github --org alice --create-remote
```

**What happens:**
1. Read org from `--org` flag: `alice`

2. Build secret path:
   ```rust
   let path = SecretPath::new("github", "alice", "token");
   ```

3. Request secret:
   ```rust
   let token = auth.get_secret(&path).await?;
   // Scope: "github/alice/*"
   ```

4. Create repo via GitHub API:
   ```rust
   github_client.create_repo(org="alice", name="my-repo", token).await?;
   ```

5. Configure git remote:
   ```bash
   git remote add origin git@github.com:alice/my-repo.git
   ```

**Secrets requested:**
- `github/alice/token` (scope: `github/alice/*`)

---

#### Example 4: Publish package (Registry operations)

```bash
hyperforge publish --path . --bump patch
```

**What happens:**
1. Detect package: `Cargo.toml` → crates.io

2. Build secret path (registries are usually user-global):
   ```rust
   let path = SecretPath::new("cargo", "token", "");
   // Or: SecretPath::parse("cargo/token")?;
   ```

3. Request secret:
   ```rust
   let token = auth.get_secret(&path).await?;
   // Scope: "cargo/*" (all cargo operations)
   ```

4. Bump version, commit, publish:
   ```rust
   cargo.bump_version(bump)?;
   git.commit("chore: bump version")?;
   cargo.publish(&token)?;
   git.tag(format!("v{}", new_version))?;
   ```

5. Push to forges (triggers forge token requests):
   ```rust
   hyperforge_push().await?;
   // Requests: github/alice/token, codeberg/alice/token
   ```

**Secrets requested:**
- `cargo/token` (scope: `cargo/*`)
- `github/alice/token` (scope: `github/alice/*`)
- `codeberg/alice/token` (scope: `codeberg/alice/*`)

---

#### Example 5: Workspace push (Multiple orgs)

```bash
cd ~/projects  # Contains repos for alice, acme-corp, bob

hyperforge workspace push --path .
```

**Workspace structure:**
```
~/projects/
  ├── alice-repo/.hyperforge/config.toml  # org = "alice"
  ├── acme-tool/.hyperforge/config.toml   # org = "acme-corp"
  └── bob-lib/.hyperforge/config.toml     # org = "bob"
```

**What happens:**
1. Discover all repos:
   ```rust
   let repos = workspace_discovery.find_repos("~/projects")?;
   // Found: alice-repo, acme-tool, bob-lib
   ```

2. For each repo, push (requests secrets as needed):
   ```rust
   for repo in repos {
       let config = read_config(&repo.path)?;
       let org = config.org;

       // Request secrets for this org
       for forge in &config.forges {
           let path = SecretPath::new(forge, &org, "token");
           let token = auth.get_secret(&path).await?;

           git.push_with_token(&repo.path, forge, &token)?;
       }
   }
   ```

**Secrets requested (dynamically discovered):**
- `github/alice/token` (scope: `github/alice/*`)
- `codeberg/alice/token`
- `github/acme-corp/token` (scope: `github/acme-corp/*`)
- `codeberg/acme-corp/token`
- `github/bob/token` (scope: `github/bob/*`)

**User sees:**
```
Requesting access to:
  github/alice/*
  codeberg/alice/*
  github/acme-corp/*
  codeberg/acme-corp/*
  github/bob/*

✓ Granted: github/alice/*
✓ Granted: codeberg/alice/*
✓ Granted: github/acme-corp/*
✓ Granted: codeberg/acme-corp/*
✗ Denied: github/bob/*

Pushing alice-repo... ✓
Pushing acme-tool... ✓
✗ Failed: bob-lib (access denied)
```

---

#### Example 6: Workspace publish (Multiple registries + orgs)

```bash
hyperforge workspace publish --path . --bump minor --deps
```

**What happens:**
1. Discover packages, build dependency graph

2. For each package (in topological order):
   ```rust
   for pkg in topological_order {
       // Determine registry
       let registry = detect_registry(&pkg.path)?;

       // Request registry token
       let registry_path = match registry {
           Registry::Cargo => SecretPath::parse("cargo/token")?,
           Registry::Npm { org: Some(org) } => SecretPath::new("npm", &org, "token"),
           Registry::Npm { org: None } => SecretPath::parse("npm/token")?,
           Registry::PyPi => SecretPath::parse("pypi/token")?,
           // ... etc
       };

       let registry_token = auth.get_secret(&registry_path).await?;

       // Publish
       registry.publish(&pkg.path, &registry_token)?;

       // Then push to forges (triggers forge token requests)
       let config = read_config(&pkg.path)?;
       for forge in &config.forges {
           let forge_path = SecretPath::new(forge, &config.org, "token");
           let forge_token = auth.get_secret(&forge_path).await?;
           git.push_with_token(&pkg.path, forge, &forge_token)?;
       }
   }
   ```

**Secrets requested:**
- `cargo/token` (scope: `cargo/*`)
- `npm/@acme-corp/token` (scope: `npm/@acme-corp/*`)
- `pypi/token` (scope: `pypi/*`)
- `github/alice/token` (scope: `github/alice/*`)
- `codeberg/alice/token`
- `github/acme-corp/token` (scope: `github/acme-corp/*`)

---

## Hyperforge Implementation Details

### Config Schema

```toml
# .hyperforge/config.toml

# Default org for all operations
org = "alice"

# Forges to mirror to
forges = ["github", "codeberg"]

# Per-forge overrides (optional)
[forges.github]
org = "acme-corp"  # Override: use acme-corp for GitHub
remote = "origin"  # Git remote name

[forges.codeberg]
org = "alice"      # Explicit (same as default)
remote = "codeberg"

# Package metadata (auto-detected if omitted)
[package]
name = "my-crate"
registry = "cargo"
```

### Org Resolution Algorithm

```rust
fn resolve_org(config: &HyperforgeConfig, forge: &str) -> Result<String> {
    // 1. Check forge-specific override
    if let Some(forge_config) = config.forges_config.get(forge) {
        if let Some(org) = &forge_config.org {
            return Ok(org.clone());
        }
    }

    // 2. Use default org
    if let Some(org) = &config.org {
        return Ok(org.clone());
    }

    // 3. Fallback: extract from git remote
    let remote = git.get_remote_for_forge(forge)?;
    let org = extract_org_from_url(&remote.url)?;
    Ok(org)
}

fn extract_org_from_url(url: &str) -> Result<String> {
    // git@github.com:alice/my-repo.git → alice
    // https://github.com/alice/my-repo.git → alice

    let re = Regex::new(r"[:/]([^/]+)/[^/]+\.git")?;
    let captures = re.captures(url)
        .ok_or_else(|| anyhow!("Cannot extract org from URL: {}", url))?;

    Ok(captures[1].to_string())
}
```

### Secret Path Construction

```rust
impl Hyperforge {
    fn forge_token_path(&self, forge: &str) -> Result<SecretPath> {
        let org = self.resolve_org(forge)?;
        Ok(SecretPath::new(forge, &org, "token"))
    }

    fn registry_token_path(&self, registry: &Registry) -> Result<SecretPath> {
        match registry {
            Registry::Cargo => {
                // Cargo is user-global
                SecretPath::parse("cargo/token")
            }
            Registry::Npm { org: Some(org) } => {
                // npm org tokens
                SecretPath::new("npm", org, "token")
            }
            Registry::Npm { org: None } => {
                // npm user token
                SecretPath::parse("npm/token")
            }
            Registry::PyPi => {
                SecretPath::parse("pypi/token")
            }
            Registry::Hex => {
                SecretPath::parse("hex/token")
            }
            Registry::Hackage => {
                SecretPath::parse("hackage/token")
            }
        }
    }
}
```

### Auth Error Handling

```rust
impl Hyperforge {
    async fn push(&self, path: &Path) -> Result<()> {
        let config = self.read_config(path)?;

        for forge in &config.forges {
            let token_path = self.forge_token_path(forge)?;

            // Request token (may trigger scope request)
            let token = match self.auth.get_secret(&token_path).await {
                Ok(secret) => secret.value,
                Err(AuthError::AccessDenied { scope }) => {
                    eprintln!("✗ Access denied to {}", scope);
                    eprintln!("  Run: hyperforge auth grant {}", scope);
                    continue;  // Skip this forge
                }
                Err(AuthError::Timeout) => {
                    eprintln!("✗ Auth request timed out");
                    eprintln!("  Check auth-service is running");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            // Push with token
            self.git.push_with_token(path, forge, &token).await?;
            println!("✓ Pushed to {}", forge);
        }

        Ok(())
    }
}
```

---

## Auth Plugin RPC Protocol (Implementation Hole)

The auth abstraction communicates with auth-service via **[TO BE DEFINED]** protocol.

### Options (TBD):

1. **gRPC** - Typed, performant, but requires protobuf
2. **HTTP/REST** - Simple, widespread, but verbose
3. **MessagePack RPC** - Compact, fast
4. **Unix socket** - Local-only, very fast
5. **Service-native RPC** - Leverage substrate's built-in RPC

### Interface (regardless of protocol):

```
# Request scope
Request:
  master_token: string
  scope: { prefix: string, permissions: {read: bool, write: bool} }

Response:
  access_token: string
  expires_at: timestamp
  OR
  error: { code: "denied" | "timeout" | "invalid_token" }

# Get secret
Request:
  access_token: string
  path: string

Response:
  value: string
  created_at: timestamp
  updated_at: timestamp
  OR
  error: { code: "not_found" | "invalid_token" | "insufficient_scope" }
```

---

## Auth Service Backend (Implementation Hole)

How auth-service actually manages scopes and secrets is **[TO BE DEFINED]**.

### Possible implementations:

#### Option A: WorkOS Backend
```rust
struct WorkOsAuthBackend {
    client: WorkOsClient,
    // Uses WorkOS Vault for secret storage
    // Uses WorkOS auth for scope management
}
```

#### Option B: Local Backend
```rust
struct LocalAuthBackend {
    storage: Box<dyn SecretStorage>,  // Keychain, pass, file
    policy: ScopePolicy,              // Auto-grant, prompt user, etc.
}
```

#### Option C: Multi-Backend Router
```rust
struct AuthRouter {
    backends: HashMap<String, Box<dyn AuthBackend>>,
    routing: HashMap<String, String>,  // "github/alice/*" -> "workos"
}
```

**Decision deferred** - hyperforge doesn't care which backend is used.

---

## Token Caching Strategy

Auth client caches access tokens to avoid repeated scope requests:

```rust
struct AuthCache {
    tokens: HashMap<SecretScope, CachedToken>,
    secrets: HashMap<SecretPath, CachedSecret>,
}

struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

struct CachedSecret {
    value: String,
    cached_at: DateTime<Utc>,
    ttl: Duration,  // How long to cache secrets
}

impl AuthClient {
    async fn get_secret(&self, path: &SecretPath) -> Result<Secret> {
        // 1. Check secret cache
        if let Some(cached) = self.cache.get_secret(path) {
            if !cached.is_expired() {
                return Ok(cached.into());
            }
        }

        // 2. Check token cache
        let scope = path.parent_scope();
        let token = if let Some(cached) = self.cache.get_token(&scope) {
            if !cached.is_expired() {
                cached.access_token
            } else {
                self.request_fresh_token(scope).await?
            }
        } else {
            self.request_fresh_token(scope).await?
        };

        // 3. Fetch from storage
        let secret = self.fetch_from_storage(&token, path).await?;

        // 4. Cache
        self.cache.store_secret(path, &secret);

        Ok(secret)
    }
}
```

**Cache TTL:**
- Access tokens: Until expiry (from token response)
- Secrets: 5 minutes (avoid stale tokens)

---

## Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Access denied to scope: {scope}")]
    AccessDenied { scope: String },

    #[error("Auth request timed out after {timeout:?}")]
    Timeout { timeout: Duration },

    #[error("Invalid master token")]
    InvalidMasterToken,

    #[error("Invalid access token")]
    InvalidAccessToken,

    #[error("Insufficient scope for {path}. Have: {have}, need: {need}")]
    InsufficientScope {
        path: String,
        have: String,
        need: String,
    },

    #[error("Secret not found: {path}")]
    SecretNotFound { path: String },

    #[error("Auth backend error: {0}")]
    BackendError(String),
}
```

---

## Dependency on Auth

**Hyperforge depends on auth from day one.**

### Ticket Dependencies:

```
AUTH-1: Auth abstraction (trait, types)
   ↓
AUTH-2: Auth client implementation (RPC to auth-service)
   ↓
LFORGE2-2: Core hyperforge types
   ↓
LFORGE2-3: Git integration
   ↓
LFORGE2-4: Init command (no auth needed yet)
   ↓
LFORGE2-7: Push command (NEEDS auth for forge tokens)
   ↓
PKG-10: Publish command (NEEDS auth for registry tokens)
```

**Critical path:**
- AUTH tickets must complete before LFORGE2-7 (push)
- Auth not needed for LFORGE2-4 (init), LFORGE2-5 (sync), LFORGE2-6 (status)

---

## Summary

### What's Defined:

✅ **Auth abstraction** - `AuthClient` trait
✅ **Secret paths** - `{forge|registry}/{org}/token`
✅ **Org scoping** - All secrets under org
✅ **Lazy requests** - Request scopes on-demand
✅ **Hyperforge integration** - How hyperforge uses auth
✅ **Error handling** - Auth error types

### What's Deferred (Implementation Holes):

⏸ **RPC protocol** - How auth client talks to auth-service
⏸ **Backend implementation** - WorkOS vs local vs multi
⏸ **Scope granting** - Auto vs user approval vs policy
⏸ **Master token management** - Login flow, refresh
⏸ **Secret storage** - Where secrets actually live
⏸ **Multi-user** - How multiple users share same machine

### Next Steps:

1. **Implement AUTH-1**: Auth abstraction (trait + types)
2. **Stub AUTH-2**: Auth client with mock backend
3. **Build LFORGE2-2, 3, 4**: Core hyperforge (no auth)
4. **Implement AUTH-2 fully**: Real RPC to auth-service
5. **Build LFORGE2-7**: Push command (uses auth)
6. **Implement auth-service backend**: WorkOS or local

---

## Open Questions

1. **Where does master token come from?**
   - User runs `synapse login` at startup?
   - Stored in `~/.synapse/token`?
   - Refreshed automatically?

2. **How are scopes granted?**
   - Auto-grant all scopes?
   - User approves each scope once?
   - Policy-based (trust hyperforge, deny others)?

3. **Multi-user on same machine?**
   - Each user has own WorkOS account?
   - Switch accounts with `synapse use-account alice`?
   - Or always single-user local dev?

4. **CI/CD scenarios?**
   - Service account with pre-granted scopes?
   - Environment variable with master token?
   - Different backend for CI vs local?

**Answers deferred** - auth-service implementation will address these.
