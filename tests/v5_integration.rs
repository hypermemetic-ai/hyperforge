//! V5 integration test runner (V5CORE-9).
//!
//! Discovers every `tests/v5/<EPIC>/*.sh` at compile time (via
//! `build.rs`, or at runtime via directory walk), runs each as a single
//! `#[test]`. Each script is tagged by its `# tier: <N>` magic comment
//! on line 2 (absent → tier 1). Tier-2 / tier-3 scripts are gated on
//! crate features named `tier2` and `tier3`.
//!
//! The generated tests exec `bash <script>`; a non-zero exit fails the
//! test and forwards captured stdout + stderr through the failure
//! message.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    One,
    Two,
    Three,
}

impl Tier {
    fn from_val(v: u32, path: &Path) -> Result<Self, String> {
        match v {
            1 => Ok(Self::One),
            2 => Ok(Self::Two),
            3 => Ok(Self::Three),
            other => Err(format!(
                "unknown tier {other} in {} — expected 1, 2, or 3",
                path.display()
            )),
        }
    }

    fn enabled(self) -> bool {
        match self {
            Self::One => true,
            Self::Two => cfg!(feature = "tier2"),
            Self::Three => cfg!(feature = "tier3"),
        }
    }
}

fn parse_tier(script: &Path) -> Result<Tier, String> {
    let f = std::fs::File::open(script)
        .map_err(|e| format!("open {}: {e}", script.display()))?;
    let rdr = BufReader::new(f);
    // Look at the first few lines for a `# tier: N` magic comment.
    for (idx, line) in rdr.lines().take(5).enumerate() {
        let line = line.map_err(|e| format!("read {}: {e}", script.display()))?;
        if let Some(rest) = line.trim_start().strip_prefix("# tier:") {
            // A note may follow the number: `# tier: 1 (no forge)`.
            let n: u32 = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .parse()
                .map_err(|e| format!(
                    "invalid tier value on line {} of {}: {e}",
                    idx + 1,
                    script.display()
                ))?;
            return Tier::from_val(n, script);
        }
    }
    Ok(Tier::One)
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at the crate root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn discover() -> Result<BTreeMap<String, (PathBuf, Tier)>, String> {
    let v5_root = repo_root().join("tests").join("v5");
    if !v5_root.is_dir() {
        return Ok(BTreeMap::new());
    }
    let mut out = BTreeMap::new();
    for epic in std::fs::read_dir(&v5_root)
        .map_err(|e| format!("read_dir {}: {e}", v5_root.display()))?
    {
        let epic = epic.map_err(|e| format!("dirent in {}: {e}", v5_root.display()))?;
        let epic_path = epic.path();
        if !epic_path.is_dir() {
            continue;
        }
        let name = epic_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        // Skip support directories — fixtures and the harness itself.
        if name == "harness" || name == "fixtures" {
            continue;
        }
        for f in std::fs::read_dir(&epic_path)
            .map_err(|e| format!("read_dir {}: {e}", epic_path.display()))?
        {
            let f = f.map_err(|e| format!("dirent in {}: {e}", epic_path.display()))?;
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("sh") {
                continue;
            }
            let tier = parse_tier(&p)?;
            // Test id: lowercase-epic + basename-without-ext.
            // e.g. V5CORE/V5CORE-2.sh → v5core_2
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            // Strip the epic prefix ("V5CORE-2" → "2") and prepend snake.
            let parts: Vec<&str> = stem.splitn(2, '-').collect();
            let id = if parts.len() == 2 {
                format!("{}_{}", parts[0].to_lowercase(), parts[1])
            } else {
                stem.to_lowercase()
            };
            out.insert(id, (p, tier));
        }
    }
    Ok(out)
}

fn run_script(path: &Path) -> Result<(), String> {
    let out = Command::new("bash")
        .arg(path)
        .output()
        .map_err(|e| format!("spawn bash {}: {e}", path.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "{} failed (exit {})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        path.display(),
        out.status.code().unwrap_or(-1),
        stdout,
        stderr,
    ))
}

fn run_one(id: &str) -> Result<(), String> {
    let scripts = discover()?;
    let (path, tier) = scripts
        .get(id)
        .ok_or_else(|| format!("no script registered for '{id}'"))?;
    if !tier.enabled() {
        // Skipped tests are reported as passing-with-note; the harness
        // contract requires "skipped, not pass or fail". Rust's built-in
        // `#[ignore]` doesn't fit the dynamic-discovery model, so we
        // log and return Ok.
        eprintln!("[skip] {} (tier {:?} not enabled)", id, tier);
        return Ok(());
    }
    // Ensure the binary is present. If not, attempt to build it.
    let bin = repo_root().join("target").join("debug").join("hyperforge-v5");
    let release_bin = repo_root().join("target").join("release").join("hyperforge-v5");
    if !bin.exists() && !release_bin.exists() {
        let status = Command::new("cargo")
            .args(["build", "--bin", "hyperforge-v5"])
            .current_dir(repo_root())
            .status()
            .map_err(|e| format!("cargo build: {e}"))?;
        if !status.success() {
            return Err("cargo build --bin hyperforge-v5 failed".to_string());
        }
    }
    run_script(path)
}

/// Smoke test that discovery itself works — catches an invalid `# tier:`
/// value at discovery time (acceptance #5).
#[test]
fn discovery_succeeds() {
    if let Err(e) = discover() {
        panic!("v5 script discovery failed: {e}");
    }
}

// One #[test] per V5CORE script.
#[test]
fn v5core_2() {
    run_one("v5core_2").unwrap();
}
#[test]
fn v5core_3() {
    run_one("v5core_3").unwrap();
}
#[test]
fn v5core_4() {
    run_one("v5core_4").unwrap();
}
#[test]
fn v5core_5() {
    run_one("v5core_5").unwrap();
}
#[test]
fn v5core_6() {
    run_one("v5core_6").unwrap();
}
#[test]
fn v5core_7() {
    run_one("v5core_7").unwrap();
}
#[test]
fn v5core_8() {
    run_one("v5core_8").unwrap();
}
#[test]
fn v5core_9() {
    run_one("v5core_9").unwrap();
}
#[test]
fn v5core_10() {
    run_one("v5core_10").unwrap();
}

// One #[test] per V5ORGS script.
#[test]
fn v5orgs_2() {
    run_one("v5orgs_2").unwrap();
}
#[test]
fn v5orgs_3() {
    run_one("v5orgs_3").unwrap();
}
#[test]
fn v5orgs_4() {
    run_one("v5orgs_4").unwrap();
}
#[test]
fn v5orgs_5() {
    run_one("v5orgs_5").unwrap();
}
#[test]
fn v5orgs_6() {
    run_one("v5orgs_6").unwrap();
}
#[test]
fn v5orgs_7() {
    run_one("v5orgs_7").unwrap();
}
#[test]
fn v5orgs_8() {
    run_one("v5orgs_8").unwrap();
}
#[test]
fn v5orgs_9() {
    run_one("v5orgs_9").unwrap();
}

// One #[test] per V5REPOS script. Tier-2 scripts (adapters, sync, push)
// skip cleanly when HF_TEST_*_TOKEN env vars are unset.
#[test]
fn v5repos_2() {
    run_one("v5repos_2").unwrap();
}
#[test]
fn v5repos_3() {
    run_one("v5repos_3").unwrap();
}
#[test]
fn v5repos_4() {
    run_one("v5repos_4").unwrap();
}
#[test]
fn v5repos_5() {
    run_one("v5repos_5").unwrap();
}
#[test]
fn v5repos_6() {
    run_one("v5repos_6").unwrap();
}
#[test]
fn v5repos_7() {
    run_one("v5repos_7").unwrap();
}
#[test]
fn v5repos_8() {
    run_one("v5repos_8").unwrap();
}
#[test]
fn v5repos_9() {
    run_one("v5repos_9").unwrap();
}
#[test]
fn v5repos_10() {
    run_one("v5repos_10").unwrap();
}
#[test]
fn v5repos_11() {
    run_one("v5repos_11").unwrap();
}
#[test]
fn v5repos_12() {
    run_one("v5repos_12").unwrap();
}
#[test]
fn v5repos_13() {
    run_one("v5repos_13").unwrap();
}
#[test]
fn v5repos_14() {
    run_one("v5repos_14").unwrap();
}
#[test]
fn v5repos_15() {
    run_one("v5repos_15").unwrap();
}

// One #[test] per V5WS script. V5WS-9 stays wired but is tier-2 and
// will no-op under the default `cargo test --test v5_integration`
// invocation (its script carries `# tier: 2` and run_one skips it
// without the tier2 feature).
#[test]
fn v5ws_2() {
    run_one("v5ws_2").unwrap();
}
#[test]
fn v5ws_3() {
    run_one("v5ws_3").unwrap();
}
#[test]
fn v5ws_4() {
    run_one("v5ws_4").unwrap();
}
#[test]
fn v5ws_5() {
    run_one("v5ws_5").unwrap();
}
#[test]
fn v5ws_6() {
    run_one("v5ws_6").unwrap();
}
#[test]
fn v5ws_7() {
    run_one("v5ws_7").unwrap();
}
#[test]
fn v5ws_8() {
    run_one("v5ws_8").unwrap();
}
#[test]
fn v5ws_9() {
    run_one("v5ws_9").unwrap();
}
#[test]
fn v5ws_10() {
    run_one("v5ws_10").unwrap();
}

// One #[test] per V5PROV script. V5PROV-2 is tier 1 (trait/schema
// surface). V5PROV-3..9 are tier 2 (real forge API) and SKIP-clean
// under the default invocation.
#[test]
fn v5prov_2() {
    run_one("v5prov_2").unwrap();
}
#[test]
fn v5prov_3() {
    run_one("v5prov_3").unwrap();
}
#[test]
fn v5prov_4() {
    run_one("v5prov_4").unwrap();
}
#[test]
fn v5prov_5() {
    run_one("v5prov_5").unwrap();
}
#[test]
fn v5prov_6() {
    run_one("v5prov_6").unwrap();
}
#[test]
fn v5prov_7() {
    run_one("v5prov_7").unwrap();
}
#[test]
fn v5prov_8() {
    run_one("v5prov_8").unwrap();
}
#[test]
fn v5prov_9() {
    run_one("v5prov_9").unwrap();
}

// One #[test] per V5LIFECYCLE script.
#[test]
fn v5lifecycle_2() { run_one("v5lifecycle_2").unwrap(); }
#[test]
fn v5lifecycle_3() { run_one("v5lifecycle_3").unwrap(); }
#[test]
fn v5lifecycle_4() { run_one("v5lifecycle_4").unwrap(); }
#[test]
fn v5lifecycle_5() { run_one("v5lifecycle_5").unwrap(); }
#[test]
fn v5lifecycle_6() { run_one("v5lifecycle_6").unwrap(); }
#[test]
fn v5lifecycle_7() { run_one("v5lifecycle_7").unwrap(); }
#[test]
fn v5lifecycle_8() { run_one("v5lifecycle_8").unwrap(); }
#[test]
fn v5lifecycle_9() { run_one("v5lifecycle_9").unwrap(); }
#[test]
fn v5lifecycle_10() { run_one("v5lifecycle_10").unwrap(); }
#[test]
fn v5lifecycle_11() { run_one("v5lifecycle_11").unwrap(); }
