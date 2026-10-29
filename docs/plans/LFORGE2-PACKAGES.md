# LFORGE2-PACKAGES: Unified Package Publishing

**Status**: Planning
**Sub-Epic**: Package Publishing Infrastructure
**Parent**: LFORGE2-1
**Goal**: Unified, safe, dependency-aware package publishing across all ecosystems

---

## Overview

Publishing packages is complex:
- **5 different ecosystems**: Rust, Node, Elixir, Python, Haskell
- **5 different registries**: crates.io, npm, hex.pm, PyPI, Hackage
- **5 different manifest formats**: TOML, JSON, Elixir, Python, Cabal
- **Different auth mechanisms**: tokens, credentials files, login flows
- **Dependency management**: Update dependents when publishing libraries
- **Transactional semantics**: All-or-nothing for workspace publishes

---

## Key Requirements

### 1. Unified Interface
```rust
// One trait, multiple implementations
trait PackageRegistry {
    async fn detect(&self, path: &Path) -> Option<PackageInfo>;
    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String>;
    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult>;
    async fn update_dependency(&self, path: &Path, dep: &str, version: &str) -> Result<()>;
}
```

### 2. Dependency-Aware Publishing
```bash
# Workspace with interdependent packages:
workspace/
  ├── core/       # 1.0.0, no local deps
  ├── cli/        # depends on core ^1.0
  └── plugins/    # depends on core ^1.0

# After publish --bump minor:
workspace/
  ├── core/       # 1.1.0 (published first)
  ├── cli/        # depends on core ^1.1 (updated, then published)
  └── plugins/    # depends on core ^1.1 (updated, then published)
```

### 3. Transactional Guarantees
- If any package fails to publish, roll back version bumps
- Don't leave workspace in inconsistent state
- Clear error reporting: which package failed, why

### 4. Registry Authentication
- Each registry has different auth (separate from forge auth)
- Request credentials from auth-service
- Cache for session
- Clear errors when missing auth

---

## Tickets

### PKG-1: PackageRegistry Trait

**Goal**: Define unified interface for all package registries

**Implementation**:
```rust
// src/package/registry.rs

#[async_trait]
pub trait PackageRegistry: Send + Sync {
    /// Detect if this registry handles packages in this directory
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>>;

    /// Bump version in manifest file (patch/minor/major)
    async fn bump_version(
        &self,
        path: &Path,
        bump: VersionBump,
    ) -> Result<String>;  // Returns new version

    /// Publish package to registry
    async fn publish(
        &self,
        path: &Path,
        dry_run: bool,
    ) -> Result<PublishResult>;

    /// Update a dependency version in manifest
    async fn update_dependency(
        &self,
        path: &Path,
        dep_name: &str,
        new_version: &str,
    ) -> Result<()>;

    /// Get current version from manifest
    async fn get_version(&self, path: &Path) -> Result<String>;

    /// Registry name (for error messages)
    fn name(&self) -> &str;
}

pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub registry: String,  // "crates.io", "npm", etc.
    pub manifest_path: PathBuf,
    pub dependencies: Vec<Dependency>,
}

pub struct Dependency {
    pub name: String,
    pub version_req: String,  // "^1.0", ">=2.0.0", etc.
    pub is_local: bool,       // true if path dependency
    pub local_path: Option<PathBuf>,
}

pub enum VersionBump {
    Patch,  // 1.2.3 → 1.2.4
    Minor,  // 1.2.3 → 1.3.0
    Major,  // 1.2.3 → 2.0.0
}

pub struct PublishResult {
    pub package: String,
    pub version: String,
    pub registry: String,
    pub url: Option<String>,  // Package URL if known
}
```

**Acceptance**:
- Trait compiles
- Documentation clear
- All methods have clear semantics

**Blocked by**: LFORGE2-2 (need core types)
**Unlocks**: PKG-2, PKG-3, PKG-4, PKG-5, PKG-6

---

### PKG-2: Cargo Registry (Rust)

**Goal**: Implement PackageRegistry for Rust/crates.io

**Implementation**:
```rust
// src/package/registries/cargo.rs

pub struct CargoRegistry {
    auth: Arc<dyn AuthProvider>,
}

impl CargoRegistry {
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>> {
        let manifest = path.join("Cargo.toml");
        if !manifest.exists() {
            return Ok(None);
        }

        // Parse Cargo.toml with toml_edit (preserves formatting)
        let content = fs::read_to_string(&manifest)?;
        let doc = content.parse::<toml_edit::Document>()?;

        let name = doc["package"]["name"].as_str()?.to_string();
        let version = doc["package"]["version"].as_str()?.to_string();

        // Parse dependencies
        let deps = self.parse_dependencies(&doc)?;

        Ok(Some(PackageInfo {
            name,
            version,
            registry: "crates.io".to_string(),
            manifest_path: manifest,
            dependencies: deps,
        }))
    }

    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String> {
        let manifest = path.join("Cargo.toml");
        let content = fs::read_to_string(&manifest)?;
        let mut doc = content.parse::<toml_edit::Document>()?;

        let current = doc["package"]["version"].as_str()?;
        let new_version = bump.apply(current)?;

        doc["package"]["version"] = toml_edit::value(new_version.clone());
        fs::write(&manifest, doc.to_string())?;

        Ok(new_version)
    }

    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult> {
        // Get cargo token from auth
        let token = self.auth.request("cargo-token").await?;

        // Run cargo publish
        let mut cmd = Command::new("cargo");
        cmd.arg("publish")
           .current_dir(path);

        if dry_run {
            cmd.arg("--dry-run");
        }

        cmd.env("CARGO_REGISTRY_TOKEN", &token);

        let output = cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow!("cargo publish failed: {}",
                String::from_utf8_lossy(&output.stderr)));
        }

        let info = self.detect(path).await?.unwrap();

        Ok(PublishResult {
            package: info.name.clone(),
            version: info.version.clone(),
            registry: "crates.io".to_string(),
            url: Some(format!("https://crates.io/crates/{}", info.name)),
        })
    }

    async fn update_dependency(&self, path: &Path, dep: &str, version: &str) -> Result<()> {
        let manifest = path.join("Cargo.toml");
        let content = fs::read_to_string(&manifest)?;
        let mut doc = content.parse::<toml_edit::Document>()?;

        // Update in dependencies section
        if let Some(deps) = doc.get_mut("dependencies") {
            if let Some(dep_entry) = deps.get_mut(dep) {
                if dep_entry.is_str() {
                    *dep_entry = toml_edit::value(version);
                } else if let Some(table) = dep_entry.as_inline_table_mut() {
                    table.insert("version", version.into());
                }
            }
        }

        // Also check dev-dependencies, build-dependencies
        // ... similar logic

        fs::write(&manifest, doc.to_string())?;
        Ok(())
    }
}
```

**Testing**:
- Detect Cargo.toml correctly
- Parse dependencies (including path deps)
- Bump version preserving formatting
- Update dependency versions
- Dry-run publish works
- Real publish works (manual test)

**Blocked by**: PKG-1
**Unlocks**: PKG-7 (multi-registry detection)

---

### PKG-3: NPM Registry (JavaScript/TypeScript)

**Goal**: Implement PackageRegistry for npm

**Implementation**:
```rust
// src/package/registries/npm.rs

pub struct NpmRegistry {
    auth: Arc<dyn AuthProvider>,
}

impl NpmRegistry {
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>> {
        let manifest = path.join("package.json");
        if !manifest.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&manifest)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;

        let name = json["name"].as_str()?.to_string();
        let version = json["version"].as_str()?.to_string();

        let deps = self.parse_dependencies(&json)?;

        Ok(Some(PackageInfo {
            name,
            version,
            registry: "npm".to_string(),
            manifest_path: manifest,
            dependencies: deps,
        }))
    }

    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String> {
        let manifest = path.join("package.json");
        let content = fs::read_to_string(&manifest)?;
        let mut json: serde_json::Value = serde_json::from_str(&content)?;

        let current = json["version"].as_str()?;
        let new_version = bump.apply(current)?;

        json["version"] = serde_json::Value::String(new_version.clone());

        // Write with pretty formatting
        let formatted = serde_json::to_string_pretty(&json)?;
        fs::write(&manifest, formatted)?;

        Ok(new_version)
    }

    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult> {
        // NPM uses .npmrc or npm login
        // We can set //registry.npmjs.org/:_authToken in .npmrc
        let token = self.auth.request("npm-token").await?;

        let npmrc = path.join(".npmrc");
        let npmrc_content = format!("//registry.npmjs.org/:_authToken={}", token);
        fs::write(&npmrc, npmrc_content)?;

        let mut cmd = Command::new("npm");
        cmd.arg("publish")
           .current_dir(path);

        if dry_run {
            cmd.arg("--dry-run");
        }

        let output = cmd.output().await?;

        // Clean up .npmrc
        let _ = fs::remove_file(&npmrc);

        if !output.status.success() {
            return Err(anyhow!("npm publish failed: {}",
                String::from_utf8_lossy(&output.stderr)));
        }

        let info = self.detect(path).await?.unwrap();

        Ok(PublishResult {
            package: info.name.clone(),
            version: info.version.clone(),
            registry: "npm".to_string(),
            url: Some(format!("https://www.npmjs.com/package/{}", info.name)),
        })
    }

    async fn update_dependency(&self, path: &Path, dep: &str, version: &str) -> Result<()> {
        let manifest = path.join("package.json");
        let content = fs::read_to_string(&manifest)?;
        let mut json: serde_json::Value = serde_json::from_str(&content)?;

        // Update in dependencies, devDependencies, peerDependencies
        for section in ["dependencies", "devDependencies", "peerDependencies"] {
            if let Some(deps) = json.get_mut(section) {
                if let Some(obj) = deps.as_object_mut() {
                    if obj.contains_key(dep) {
                        obj.insert(dep.to_string(), serde_json::Value::String(version.to_string()));
                    }
                }
            }
        }

        let formatted = serde_json::to_string_pretty(&json)?;
        fs::write(&manifest, formatted)?;
        Ok(())
    }
}
```

**Testing**: Same as PKG-2 but for npm

**Blocked by**: PKG-1
**Unlocks**: PKG-7

---

### PKG-4: Hex Registry (Elixir)

**Goal**: Implement PackageRegistry for hex.pm

**Implementation**:
```rust
// src/package/registries/hex.rs

pub struct HexRegistry {
    auth: Arc<dyn AuthProvider>,
}

impl HexRegistry {
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>> {
        let manifest = path.join("mix.exs");
        if !manifest.exists() {
            return Ok(None);
        }

        // Parse mix.exs as Elixir code
        // This is tricky - might need to run `mix help` or regex parse
        let content = fs::read_to_string(&manifest)?;

        // Regex to find: version: "1.2.3"
        let version_re = Regex::new(r#"version:\s*"([^"]+)""#)?;
        let version = version_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| anyhow!("Could not find version in mix.exs"))?;

        // Regex to find: app: :my_app
        let app_re = Regex::new(r#"app:\s*:(\w+)"#)?;
        let name = app_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| anyhow!("Could not find app name in mix.exs"))?;

        Ok(Some(PackageInfo {
            name,
            version,
            registry: "hex.pm".to_string(),
            manifest_path: manifest,
            dependencies: vec![],  // TODO: parse deps
        }))
    }

    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String> {
        let manifest = path.join("mix.exs");
        let content = fs::read_to_string(&manifest)?;

        let version_re = Regex::new(r#"version:\s*"([^"]+)""#)?;
        let current = version_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .ok_or_else(|| anyhow!("Could not find version"))?;

        let new_version = bump.apply(current)?;

        // Replace version in file
        let new_content = version_re.replace(&content,
            format!(r#"version: "{}""#, new_version));

        fs::write(&manifest, new_content.as_ref())?;
        Ok(new_version)
    }

    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult> {
        // Hex uses `mix hex.publish`
        // Auth is via `mix hex.user auth` (stores in ~/.hex)

        if dry_run {
            // No dry-run for hex, just skip publish
            let info = self.detect(path).await?.unwrap();
            return Ok(PublishResult {
                package: info.name,
                version: info.version,
                registry: "hex.pm".to_string(),
                url: None,
            });
        }

        let mut cmd = Command::new("mix");
        cmd.arg("hex.publish")
           .arg("--yes")  // Skip confirmation
           .current_dir(path);

        let output = cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow!("mix hex.publish failed: {}",
                String::from_utf8_lossy(&output.stderr)));
        }

        let info = self.detect(path).await?.unwrap();

        Ok(PublishResult {
            package: info.name.clone(),
            version: info.version.clone(),
            registry: "hex.pm".to_string(),
            url: Some(format!("https://hex.pm/packages/{}", info.name)),
        })
    }
}
```

**Testing**: Same pattern as PKG-2, PKG-3

**Blocked by**: PKG-1
**Unlocks**: PKG-7

---

### PKG-5: PyPI Registry (Python)

**Goal**: Implement PackageRegistry for PyPI

**Implementation**:
```rust
// src/package/registries/pypi.rs

pub struct PyPiRegistry {
    auth: Arc<dyn AuthProvider>,
}

impl PyPiRegistry {
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>> {
        // Check for pyproject.toml first, then setup.py
        let pyproject = path.join("pyproject.toml");
        let setup_py = path.join("setup.py");

        if pyproject.exists() {
            return self.detect_pyproject(&pyproject).await;
        } else if setup_py.exists() {
            return self.detect_setup_py(&setup_py).await;
        }

        Ok(None)
    }

    async fn detect_pyproject(&self, path: &Path) -> Result<Option<PackageInfo>> {
        let content = fs::read_to_string(path)?;
        let doc: toml::Value = toml::from_str(&content)?;

        let name = doc["project"]["name"].as_str()?.to_string();
        let version = doc["project"]["version"].as_str()?.to_string();

        Ok(Some(PackageInfo {
            name,
            version,
            registry: "pypi".to_string(),
            manifest_path: path.to_path_buf(),
            dependencies: vec![],
        }))
    }

    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String> {
        let pyproject = path.join("pyproject.toml");
        if pyproject.exists() {
            return self.bump_pyproject(&pyproject, bump).await;
        }

        // For setup.py, would need to parse Python code (harder)
        Err(anyhow!("Version bumping only supported for pyproject.toml"))
    }

    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult> {
        // Build package
        let mut build_cmd = Command::new("python");
        build_cmd.arg("-m")
                 .arg("build")
                 .current_dir(path);

        let output = build_cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!("python -m build failed"));
        }

        if dry_run {
            let info = self.detect(path).await?.unwrap();
            return Ok(PublishResult {
                package: info.name,
                version: info.version,
                registry: "pypi".to_string(),
                url: None,
            });
        }

        // Upload with twine
        let token = self.auth.request("pypi-token").await?;

        let mut upload_cmd = Command::new("twine");
        upload_cmd.arg("upload")
                  .arg("dist/*")
                  .current_dir(path)
                  .env("TWINE_USERNAME", "__token__")
                  .env("TWINE_PASSWORD", &token);

        let output = upload_cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow!("twine upload failed: {}",
                String::from_utf8_lossy(&output.stderr)));
        }

        let info = self.detect(path).await?.unwrap();

        Ok(PublishResult {
            package: info.name.clone(),
            version: info.version.clone(),
            registry: "pypi".to_string(),
            url: Some(format!("https://pypi.org/project/{}", info.name)),
        })
    }
}
```

**Testing**: Same pattern

**Blocked by**: PKG-1
**Unlocks**: PKG-7

---

### PKG-6: Hackage Registry (Haskell)

**Goal**: Implement PackageRegistry for Hackage

**Implementation**:
```rust
// src/package/registries/hackage.rs

pub struct HackageRegistry {
    auth: Arc<dyn AuthProvider>,
}

impl HackageRegistry {
    async fn detect(&self, path: &Path) -> Result<Option<PackageInfo>> {
        // Find *.cabal file
        let entries = fs::read_dir(path)?;
        let cabal_file = entries
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension() == Some(OsStr::new("cabal")));

        let Some(cabal) = cabal_file else {
            return Ok(None);
        };

        let content = fs::read_to_string(cabal.path())?;

        // Parse cabal file (basic parsing)
        let name_re = Regex::new(r"(?m)^name:\s*(.+)$")?;
        let version_re = Regex::new(r"(?m)^version:\s*(.+)$")?;

        let name = name_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .ok_or_else(|| anyhow!("No name in cabal file"))?;

        let version = version_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .ok_or_else(|| anyhow!("No version in cabal file"))?;

        Ok(Some(PackageInfo {
            name,
            version,
            registry: "hackage".to_string(),
            manifest_path: cabal.path(),
            dependencies: vec![],
        }))
    }

    async fn bump_version(&self, path: &Path, bump: VersionBump) -> Result<String> {
        let info = self.detect(path).await?.unwrap();
        let content = fs::read_to_string(&info.manifest_path)?;

        let version_re = Regex::new(r"(?m)^version:\s*(.+)$")?;
        let current = version_re.captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim())
            .ok_or_else(|| anyhow!("No version found"))?;

        let new_version = bump.apply(current)?;

        let new_content = version_re.replace(&content,
            format!("version:        {}", new_version));

        fs::write(&info.manifest_path, new_content.as_ref())?;
        Ok(new_version)
    }

    async fn publish(&self, path: &Path, dry_run: bool) -> Result<PublishResult> {
        // Build package
        let mut cmd = Command::new("cabal");
        cmd.arg("sdist")
           .current_dir(path);

        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!("cabal sdist failed"));
        }

        if dry_run {
            let info = self.detect(path).await?.unwrap();
            return Ok(PublishResult {
                package: info.name,
                version: info.version,
                registry: "hackage".to_string(),
                url: None,
            });
        }

        // Upload
        let mut upload_cmd = Command::new("cabal");
        upload_cmd.arg("upload")
                  .arg("--publish")
                  .current_dir(path);

        let output = upload_cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow!("cabal upload failed"));
        }

        let info = self.detect(path).await?.unwrap();

        Ok(PublishResult {
            package: info.name.clone(),
            version: info.version.clone(),
            registry: "hackage".to_string(),
            url: Some(format!("https://hackage.haskell.org/package/{}", info.name)),
        })
    }
}
```

**Testing**: Same pattern

**Blocked by**: PKG-1
**Unlocks**: PKG-7

---

### PKG-7: Multi-Registry Detection

**Goal**: Auto-detect which registry to use for a given directory

**Implementation**:
```rust
// src/package/detector.rs

pub struct PackageDetector {
    registries: Vec<Box<dyn PackageRegistry>>,
}

impl PackageDetector {
    pub fn new(auth: Arc<dyn AuthProvider>) -> Self {
        Self {
            registries: vec![
                Box::new(CargoRegistry::new(auth.clone())),
                Box::new(NpmRegistry::new(auth.clone())),
                Box::new(HexRegistry::new(auth.clone())),
                Box::new(PyPiRegistry::new(auth.clone())),
                Box::new(HackageRegistry::new(auth.clone())),
            ],
        }
    }

    pub async fn detect(&self, path: &Path) -> Result<Option<DetectedPackage>> {
        for registry in &self.registries {
            if let Some(info) = registry.detect(path).await? {
                return Ok(Some(DetectedPackage {
                    info,
                    registry: registry.clone(),
                }));
            }
        }

        Ok(None)
    }

    pub async fn detect_all(&self, paths: &[PathBuf]) -> Result<Vec<DetectedPackage>> {
        let mut packages = Vec::new();

        for path in paths {
            if let Some(pkg) = self.detect(path).await? {
                packages.push(pkg);
            }
        }

        Ok(packages)
    }
}

pub struct DetectedPackage {
    pub info: PackageInfo,
    pub registry: Box<dyn PackageRegistry>,
}
```

**Testing**:
- Correctly identifies Cargo.toml → CargoRegistry
- Correctly identifies package.json → NpmRegistry
- Returns None for non-package directories

**Blocked by**: PKG-2, PKG-3, PKG-4, PKG-5, PKG-6
**Unlocks**: PKG-8, PKG-9

---

### PKG-8: Dependency Graph Construction

**Goal**: Build dependency graph from workspace packages

**Implementation**:
```rust
// src/package/graph.rs

pub struct DependencyGraph {
    packages: HashMap<String, PackageNode>,
}

pub struct PackageNode {
    pub info: PackageInfo,
    pub path: PathBuf,
    pub local_deps: Vec<String>,  // Names of local dependencies
}

impl DependencyGraph {
    pub fn build(packages: Vec<(PathBuf, PackageInfo)>) -> Result<Self> {
        let mut graph = Self {
            packages: HashMap::new(),
        };

        for (path, info) in packages {
            // Find local dependencies
            let local_deps: Vec<String> = info.dependencies.iter()
                .filter(|d| d.is_local)
                .map(|d| d.name.clone())
                .collect();

            graph.packages.insert(info.name.clone(), PackageNode {
                info,
                path,
                local_deps,
            });
        }

        // Validate no cycles
        graph.check_cycles()?;

        Ok(graph)
    }

    pub fn topological_sort(&self) -> Result<Vec<String>> {
        // Kahn's algorithm
        let mut in_degree: HashMap<String, usize> = HashMap::new();

        // Calculate in-degrees
        for (name, node) in &self.packages {
            in_degree.entry(name.clone()).or_insert(0);
            for dep in &node.local_deps {
                *in_degree.entry(dep.clone()).or_insert(0) += 1;
            }
        }

        // Start with nodes that have no dependencies
        let mut queue: Vec<String> = in_degree.iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(name, _)| name.clone())
            .collect();

        let mut result = Vec::new();

        while let Some(pkg) = queue.pop() {
            result.push(pkg.clone());

            if let Some(node) = self.packages.get(&pkg) {
                for dep in &node.local_deps {
                    if let Some(degree) = in_degree.get_mut(dep) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push(dep.clone());
                        }
                    }
                }
            }
        }

        if result.len() != self.packages.len() {
            return Err(anyhow!("Circular dependency detected"));
        }

        Ok(result)
    }

    fn check_cycles(&self) -> Result<()> {
        // DFS cycle detection
        // ... implementation
        Ok(())
    }
}
```

**Testing**:
- Build graph with no dependencies
- Build graph with linear deps (A → B → C)
- Build graph with diamond deps (C → B, C → A, B → A)
- Detect cycles (A → B → C → A)
- Topological sort returns correct order

**Blocked by**: PKG-7
**Unlocks**: PKG-10

---

### PKG-9: Registry Authentication

**Goal**: Request registry credentials from auth-service

**Implementation**:
```rust
// src/package/auth.rs

#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn request(&self, key: &str) -> Result<String>;
}

pub struct AuthServiceClient {
    access_token: String,
    timeout: Duration,
}

impl AuthServiceClient {
    pub fn new(access_token: String) -> Self {
        Self {
            access_token,
            timeout: Duration::from_secs(30),
        }
    }
}

#[async_trait]
impl AuthProvider for AuthServiceClient {
    async fn request(&self, key: &str) -> Result<String> {
        // Request credential from auth-service
        // Use access_token to authenticate request
        // Auth-service may:
        //   - Auto-approve (return immediately)
        //   - Prompt user (wait for approval)
        //   - Timeout (return error)

        // For now, stub implementation:
        // In real version, this would be RPC call to auth-service

        match key {
            "cargo-token" => {
                // Could read from ~/.cargo/credentials
                todo!("Request from auth-service")
            }
            "npm-token" => {
                // Could read from ~/.npmrc
                todo!("Request from auth-service")
            }
            _ => Err(anyhow!("Unknown credential: {}", key))
        }
    }
}
```

**Acceptance**:
- Interface defined
- Stub implementation works
- Integration with real auth-service (LFORGE2-17) deferred

**Blocked by**: PKG-1
**Unlocks**: PKG-2, PKG-3, PKG-4, PKG-5, PKG-6

---

### PKG-10: Single Package Publish Workflow

**Goal**: Publish a single package with all safeguards

**Implementation**:
```rust
// src/package/publish.rs

pub struct PublishWorkflow {
    detector: PackageDetector,
}

impl PublishWorkflow {
    pub async fn publish_single(
        &self,
        path: &Path,
        bump: VersionBump,
        dry_run: bool,
    ) -> Result<PublishResult> {
        // 1. Detect package
        let Some(detected) = self.detector.detect(path).await? else {
            return Err(anyhow!("No package found at {}", path.display()));
        };

        // 2. Check clean working tree
        self.check_clean_tree(path)?;

        // 3. Bump version
        let new_version = detected.registry.bump_version(path, bump).await?;

        // 4. Commit version bump
        if !dry_run {
            self.commit_version_bump(path, &detected.info.name, &new_version)?;
        }

        // 5. Publish
        let result = detected.registry.publish(path, dry_run).await;

        // 6. If publish failed and we committed, roll back
        if result.is_err() && !dry_run {
            self.rollback_commit(path)?;
        }

        let publish_result = result?;

        // 7. Tag release
        if !dry_run {
            self.tag_release(path, &new_version)?;
        }

        Ok(publish_result)
    }

    fn check_clean_tree(&self, path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()?;

        if !output.stdout.is_empty() {
            return Err(anyhow!("Working tree not clean. Commit changes first."));
        }

        Ok(())
    }

    fn commit_version_bump(&self, path: &Path, name: &str, version: &str) -> Result<()> {
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()?;

        Command::new("git")
            .args(["commit", "-m", &format!("chore: bump {} to {}", name, version)])
            .current_dir(path)
            .status()?;

        Ok(())
    }

    fn rollback_commit(&self, path: &Path) -> Result<()> {
        Command::new("git")
            .args(["reset", "--hard", "HEAD~1"])
            .current_dir(path)
            .status()?;

        Ok(())
    }

    fn tag_release(&self, path: &Path, version: &str) -> Result<()> {
        let tag = format!("v{}", version);

        Command::new("git")
            .args(["tag", "-a", &tag, "-m", &format!("Release {}", version)])
            .current_dir(path)
            .status()?;

        Ok(())
    }
}
```

**Testing**:
- Publish with clean tree works
- Publish with dirty tree fails
- Dry run doesn't commit/publish
- Rollback on publish failure works
- Tag is created correctly

**Blocked by**: PKG-7, PKG-9
**Unlocks**: PKG-11

---

### PKG-11: Workspace Publish with Dependencies

**Goal**: Publish all packages in workspace respecting dependency order

**Implementation**:
```rust
// src/package/workspace.rs

pub struct WorkspacePublishWorkflow {
    detector: PackageDetector,
    publish: PublishWorkflow,
}

impl WorkspacePublishWorkflow {
    pub async fn publish_workspace(
        &self,
        workspace_path: &Path,
        bump: VersionBump,
        dry_run: bool,
    ) -> Result<WorkspacePublishResult> {
        // 1. Discover all packages in workspace
        let repo_paths = self.discover_repos(workspace_path).await?;
        let packages = self.detector.detect_all(&repo_paths).await?;

        if packages.is_empty() {
            return Err(anyhow!("No packages found in workspace"));
        }

        // 2. Build dependency graph
        let package_infos: Vec<(PathBuf, PackageInfo)> = packages.iter()
            .map(|p| (p.path.clone(), p.info.clone()))
            .collect();

        let graph = DependencyGraph::build(package_infos)?;

        // 3. Get publish order (topological sort)
        let order = graph.topological_sort()?;

        // 4. Publish in order
        let mut results = Vec::new();
        let mut published_versions: HashMap<String, String> = HashMap::new();

        for pkg_name in order {
            let node = graph.packages.get(&pkg_name).unwrap();

            // Update dependencies to use newly published versions
            if !dry_run {
                for dep_name in &node.local_deps {
                    if let Some(new_version) = published_versions.get(dep_name) {
                        // Update this package's dependency to use new version
                        let registry = self.get_registry_for(&node.path)?;
                        registry.update_dependency(&node.path, dep_name, new_version).await?;
                    }
                }
            }

            // Publish this package
            match self.publish.publish_single(&node.path, bump, dry_run).await {
                Ok(result) => {
                    published_versions.insert(pkg_name.clone(), result.version.clone());
                    results.push(PackageResult::Success(result));
                }
                Err(e) => {
                    results.push(PackageResult::Failed {
                        package: pkg_name.clone(),
                        error: e.to_string(),
                    });

                    // Stop on first failure
                    break;
                }
            }
        }

        Ok(WorkspacePublishResult {
            packages: results,
            total: graph.packages.len(),
        })
    }

    async fn discover_repos(&self, workspace_path: &Path) -> Result<Vec<PathBuf>> {
        // Walk directory tree, find all .hyperforge/ dirs
        // Return their parent directories (the repo roots)
        todo!("Use LFORGE2-9 workspace discovery")
    }
}

pub struct WorkspacePublishResult {
    pub packages: Vec<PackageResult>,
    pub total: usize,
}

pub enum PackageResult {
    Success(PublishResult),
    Failed { package: String, error: String },
}
```

**Testing**:
- Workspace with no deps publishes all
- Workspace with linear deps publishes in order
- Workspace with diamond deps publishes correctly
- Failure stops remaining publishes
- Dependency versions updated correctly

**Blocked by**: PKG-8, PKG-10
**Unlocks**: [] (final ticket)

---

## Dependency DAG

```
                    PKG-1 (trait)
                         │
        ┌────────────────┼────────────┬─────────┐
        │                │            │         │
        ▼                ▼            ▼         ▼
     PKG-9            PKG-2        PKG-3     PKG-4
     (auth)          (cargo)       (npm)     (hex)
        │                │            │         │
        └────────┬───────┼────────────┼─────────┤
                 │       │            │         │
                 │       ▼            ▼         ▼
                 │    PKG-5        PKG-6     (continue)
                 │   (pypi)      (hackage)
                 │       │            │
                 │       └──────┬─────┘
                 │              │
                 │              ▼
                 │          PKG-7
                 │      (multi-detect)
                 │              │
                 │       ┌──────┴──────┐
                 │       │             │
                 │       ▼             ▼
                 │    PKG-8         (combine)
                 │  (dep graph)        │
                 │       │             │
                 └───────┴─────┬───────┘
                               │
                               ▼
                           PKG-10
                        (single pub)
                               │
                               ▼
                           PKG-11
                       (workspace pub)
```

**Critical Path**: PKG-1 → PKG-2 → PKG-7 → PKG-8 → PKG-10 → PKG-11

---

## Integration with LFORGE2

These tickets integrate with main epic:

- **PKG-7** used by **LFORGE2-13** (package detection)
- **PKG-10** used by **LFORGE2-14** (single-repo publish)
- **PKG-11** used by **LFORGE2-15** (workspace publish)
- **PKG-9** depends on **LFORGE2-17** (auth integration)

---

## Tickets Summary

| ID | Title | Blocked By | Unlocks |
|----|-------|------------|---------|
| PKG-1 | PackageRegistry trait | LFORGE2-2 | PKG-2..9 |
| PKG-2 | Cargo registry | PKG-1 | PKG-7 |
| PKG-3 | NPM registry | PKG-1 | PKG-7 |
| PKG-4 | Hex registry | PKG-1 | PKG-7 |
| PKG-5 | PyPI registry | PKG-1 | PKG-7 |
| PKG-6 | Hackage registry | PKG-1 | PKG-7 |
| PKG-7 | Multi-registry detection | PKG-2..6 | PKG-8, PKG-10 |
| PKG-8 | Dependency graph | PKG-7 | PKG-11 |
| PKG-9 | Registry auth | PKG-1 | PKG-2..6 |
| PKG-10 | Single publish workflow | PKG-7, PKG-9 | PKG-11 |
| PKG-11 | Workspace publish | PKG-8, PKG-10 | - |

**Total: 11 tickets**
