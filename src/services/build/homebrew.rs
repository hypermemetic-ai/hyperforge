//! Homebrew formula generation from release assets.
//!
//! Downloads release assets, computes sha256 checksums, and generates a valid
//! Homebrew Ruby formula mapping target triples to platform selectors.

use async_stream::stream;
use futures::Stream;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::auth::AuthProvider;
use crate::service::HyperforgeEvent;
use crate::types::Forge;

use super::release::{make_auth, make_release_adapter};

/// A matched platform asset with its download URL and sha256 hash.
struct PlatformAsset {
    url: String,
    sha256: String,
}

/// Homebrew platform selector key derived from a target triple.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum BrewPlatform {
    MacosArm,
    MacosIntel,
    LinuxArm,
    LinuxIntel,
}

impl BrewPlatform {
    /// Try to map a target triple string to a Homebrew platform.
    fn from_triple(triple: &str) -> Option<Self> {
        match triple {
            _ if triple.contains("aarch64") && triple.contains("apple-darwin") => {
                Some(Self::MacosArm)
            }
            _ if triple.contains("x86_64") && triple.contains("apple-darwin") => {
                Some(Self::MacosIntel)
            }
            _ if triple.contains("aarch64") && triple.contains("linux") => Some(Self::LinuxArm),
            _ if triple.contains("x86_64") && triple.contains("linux") => Some(Self::LinuxIntel),
            _ => None,
        }
    }
}

/// Parse a binstall-convention asset filename into (name, `target_triple`, version).
///
/// Expected format: `{name}-{target}-v{version}.{ext}`
/// e.g. `synapse-aarch64-apple-darwin-v3.10.1.tar.gz`
fn parse_asset_filename(filename: &str) -> Option<(String, String, String)> {
    // Strip known archive extensions
    let stem = filename
        .strip_suffix(".tar.gz")
        .or_else(|| filename.strip_suffix(".tgz"))
        .or_else(|| filename.strip_suffix(".tar.xz"))
        .or_else(|| filename.strip_suffix(".zip"))?;

    // Find the version component (starts with -v followed by a digit)
    let version_start = stem.rfind("-v").and_then(|idx| {
        let after = &stem[idx + 2..];
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            Some(idx)
        } else {
            None
        }
    })?;

    let version = stem[version_start + 2..].to_string();
    let name_and_target = &stem[..version_start];

    // Known target triples to match against (order: longest first to avoid partial matches)
    let known_targets = [
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ];

    for target in &known_targets {
        if let Some(prefix) = name_and_target.strip_suffix(&format!("-{target}")) {
            return Some((prefix.to_string(), target.to_string(), version));
        }
    }

    None
}

/// Convert a package name to a Ruby class name (`PascalCase`).
fn to_class_name(name: &str) -> String {
    name.split(['-', '_'])
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect()
}

/// Build the Ruby formula string from collected platform assets.
fn generate_formula(
    class_name: &str,
    description: &str,
    homepage: &str,
    version: &str,
    binary_name: &str,
    platforms: &BTreeMap<BrewPlatform, PlatformAsset>,
) -> String {
    let mut lines = Vec::new();

    lines.push(format!("class {class_name} < Formula"));
    lines.push(format!("  desc \"{description}\""));
    lines.push(format!("  homepage \"{homepage}\""));
    lines.push(format!("  version \"{version}\""));
    lines.push(String::new());

    // macOS section
    let has_macos_arm = platforms.contains_key(&BrewPlatform::MacosArm);
    let has_macos_intel = platforms.contains_key(&BrewPlatform::MacosIntel);

    if has_macos_arm || has_macos_intel {
        lines.push("  on_macos do".to_string());
        if let Some(asset) = platforms.get(&BrewPlatform::MacosArm) {
            lines.push("    on_arm do".to_string());
            lines.push(format!("      url \"{}\"", asset.url));
            lines.push(format!("      sha256 \"{}\"", asset.sha256));
            lines.push("    end".to_string());
        }
        if let Some(asset) = platforms.get(&BrewPlatform::MacosIntel) {
            lines.push("    on_intel do".to_string());
            lines.push(format!("      url \"{}\"", asset.url));
            lines.push(format!("      sha256 \"{}\"", asset.sha256));
            lines.push("    end".to_string());
        }
        lines.push("  end".to_string());
    }

    // Linux section
    let has_linux_arm = platforms.contains_key(&BrewPlatform::LinuxArm);
    let has_linux_intel = platforms.contains_key(&BrewPlatform::LinuxIntel);

    if has_linux_arm || has_linux_intel {
        if has_macos_arm || has_macos_intel {
            lines.push(String::new());
        }
        lines.push("  on_linux do".to_string());
        if let Some(asset) = platforms.get(&BrewPlatform::LinuxArm) {
            lines.push("    on_arm do".to_string());
            lines.push(format!("      url \"{}\"", asset.url));
            lines.push(format!("      sha256 \"{}\"", asset.sha256));
            lines.push("    end".to_string());
        }
        if let Some(asset) = platforms.get(&BrewPlatform::LinuxIntel) {
            lines.push("    on_intel do".to_string());
            lines.push(format!("      url \"{}\"", asset.url));
            lines.push(format!("      sha256 \"{}\"", asset.sha256));
            lines.push("    end".to_string());
        }
        lines.push("  end".to_string());
    }

    lines.push(String::new());
    lines.push("  def install".to_string());
    lines.push(format!("    bin.install \"{binary_name}\""));
    lines.push("  end".to_string());
    lines.push("end".to_string());

    lines.join("\n") + "\n"
}

/// Download an asset and compute its sha256 hex digest.
async fn download_and_hash(
    client: &reqwest::Client,
    url: &str,
    auth: &Arc<dyn AuthProvider>,
    forge: &str,
    org: &str,
) -> Result<String, String> {
    let mut request = client.get(url);

    // Add auth header for private repos
    let secret_path = format!("{forge}/{org}/token");
    if let Ok(Some(token)) = auth.get_secret(&secret_path).await {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to download {url}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Download failed for {} — HTTP {}",
            url,
            response.status()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body from {url}: {e}"))?;

    let hash = Sha256::digest(&bytes);
    Ok(format!("{hash:x}"))
}

pub fn brew_formula(
    org: String,
    name: String,
    tag: String,
    forge: Option<Forge>,
    tap_path: Option<String>,
    description: Option<String>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let is_dry_run = dry_run.unwrap_or(false);
    let forge_name = forge.map_or_else(|| "github".to_string(), |f| f.as_str().to_string());
    let desc = description.unwrap_or_else(|| format!("{name} — installed via Homebrew"));

    stream! {
        let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };

        // Set up auth and release adapter
        let auth = match make_auth() {
            Ok(a) => a,
            Err(e) => {
                yield HyperforgeEvent::Error { message: e };
                return;
            }
        };

        let adapter = match make_release_adapter(&forge_name, auth.clone(), &org) {
            Ok(a) => a,
            Err(e) => {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to create release adapter: {e}"),
                };
                return;
            }
        };

        yield HyperforgeEvent::Info {
            message: format!("{dry_prefix}Fetching release {tag} for {org}/{name} on {forge_name}"),
        };

        // Fetch the release by tag
        let release = match adapter.get_release_by_tag(&org, &name, &tag).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                yield HyperforgeEvent::Error {
                    message: format!("Release {tag} not found for {org}/{name}"),
                };
                return;
            }
            Err(e) => {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to fetch release: {e}"),
                };
                return;
            }
        };

        yield HyperforgeEvent::Info {
            message: format!("Found release {} with {} asset(s)", release.tag_name, release.assets.len()),
        };

        // Match assets to platforms
        let mut platforms: BTreeMap<BrewPlatform, PlatformAsset> = BTreeMap::new();
        let mut binary_name = name.clone();
        let mut resolved_version = tag.strip_prefix('v').unwrap_or(&tag).to_string();

        let client = reqwest::Client::builder()
            .user_agent("hyperforge/4.0")
            .build()
            .unwrap_or_default();

        let auth_provider: Arc<dyn AuthProvider> = auth;

        for asset in &release.assets {
            let parsed = if let Some(p) = parse_asset_filename(&asset.name) { p } else {
                yield HyperforgeEvent::Info {
                    message: format!("  Skipping unrecognized asset: {}", asset.name),
                };
                continue;
            };

            let (asset_name, triple, version) = parsed;
            binary_name = asset_name;
            resolved_version = version;

            let platform = if let Some(p) = BrewPlatform::from_triple(&triple) { p } else {
                yield HyperforgeEvent::Info {
                    message: format!("  Skipping unsupported platform: {} ({})", triple, asset.name),
                };
                continue;
            };

            yield HyperforgeEvent::Info {
                message: format!("{}  Downloading {} for sha256...", dry_prefix, asset.name),
            };

            if is_dry_run {
                platforms.insert(platform, PlatformAsset {
                    url: asset.download_url.clone(),
                    sha256: "dry-run-sha256-placeholder".to_string(),
                });
                continue;
            }

            match download_and_hash(&client, &asset.download_url, &auth_provider, &forge_name, &org).await {
                Ok(sha256) => {
                    yield HyperforgeEvent::Info {
                        message: format!("  {} sha256: {}", asset.name, &sha256[..12]),
                    };
                    platforms.insert(platform, PlatformAsset {
                        url: asset.download_url.clone(),
                        sha256,
                    });
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to hash {}: {}", asset.name, e),
                    };
                }
            }
        }

        if platforms.is_empty() {
            yield HyperforgeEvent::Error {
                message: "No matching platform assets found in release".to_string(),
            };
            return;
        }

        // Generate the homepage URL
        let homepage = match forge_name.as_str() {
            "github" => format!("https://github.com/{org}/{name}"),
            "codeberg" => format!("https://codeberg.org/{org}/{name}"),
            "gitlab" => format!("https://gitlab.com/{org}/{name}"),
            _ => format!("https://github.com/{org}/{name}"),
        };

        let class_name = to_class_name(&name);
        let formula = generate_formula(
            &class_name,
            &desc,
            &homepage,
            &resolved_version,
            &binary_name,
            &platforms,
        );

        // Write or emit
        if let Some(ref tap) = tap_path {
            let formula_dir = PathBuf::from(tap).join("Formula");
            let formula_file = formula_dir.join(format!("{name}.rb"));

            if is_dry_run {
                yield HyperforgeEvent::Info {
                    message: format!("{}Would write formula to {}", dry_prefix, formula_file.display()),
                };
                yield HyperforgeEvent::Info { message: formula };
            } else {
                if let Err(e) = std::fs::create_dir_all(&formula_dir) {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to create Formula directory: {e}"),
                    };
                    return;
                }

                match std::fs::write(&formula_file, &formula) {
                    Ok(()) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Wrote formula to {}", formula_file.display()),
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to write formula: {e}"),
                        };
                        return;
                    }
                }
            }
        } else {
            yield HyperforgeEvent::Info {
                message: formula,
            };
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Homebrew formula generated for {} — {} platform(s)",
                dry_prefix,
                name,
                platforms.len(),
            ),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_asset_filename_standard() {
        let (name, triple, version) =
            parse_asset_filename("synapse-aarch64-apple-darwin-v3.10.1.tar.gz").unwrap();
        assert_eq!(name, "synapse");
        assert_eq!(triple, "aarch64-apple-darwin");
        assert_eq!(version, "3.10.1");
    }

    #[test]
    fn test_parse_asset_filename_linux() {
        let (name, triple, version) =
            parse_asset_filename("hyperforge-x86_64-unknown-linux-gnu-v4.2.0.tar.gz").unwrap();
        assert_eq!(name, "hyperforge");
        assert_eq!(triple, "x86_64-unknown-linux-gnu");
        assert_eq!(version, "4.2.0");
    }

    #[test]
    fn test_parse_asset_filename_zip() {
        let (name, triple, version) =
            parse_asset_filename("tool-x86_64-pc-windows-msvc-v1.0.0.zip").unwrap();
        assert_eq!(name, "tool");
        assert_eq!(triple, "x86_64-pc-windows-msvc");
        assert_eq!(version, "1.0.0");
    }

    #[test]
    fn test_parse_asset_filename_hyphenated_name() {
        let (name, triple, version) =
            parse_asset_filename("my-cool-tool-aarch64-unknown-linux-gnu-v0.1.0.tar.gz").unwrap();
        assert_eq!(name, "my-cool-tool");
        assert_eq!(triple, "aarch64-unknown-linux-gnu");
        assert_eq!(version, "0.1.0");
    }

    #[test]
    fn test_parse_asset_filename_unrecognized() {
        assert!(parse_asset_filename("README.md").is_none());
        assert!(parse_asset_filename("checksums.txt").is_none());
    }

    #[test]
    fn test_to_class_name() {
        assert_eq!(to_class_name("synapse"), "Synapse");
        assert_eq!(to_class_name("my-tool"), "MyTool");
        assert_eq!(to_class_name("plexus-core"), "PlexusCore");
        assert_eq!(to_class_name("my_cool_tool"), "MyCoolTool");
    }

    #[test]
    fn test_brew_platform_from_triple() {
        assert_eq!(
            BrewPlatform::from_triple("aarch64-apple-darwin"),
            Some(BrewPlatform::MacosArm)
        );
        assert_eq!(
            BrewPlatform::from_triple("x86_64-apple-darwin"),
            Some(BrewPlatform::MacosIntel)
        );
        assert_eq!(
            BrewPlatform::from_triple("aarch64-unknown-linux-gnu"),
            Some(BrewPlatform::LinuxArm)
        );
        assert_eq!(
            BrewPlatform::from_triple("x86_64-unknown-linux-gnu"),
            Some(BrewPlatform::LinuxIntel)
        );
        assert_eq!(
            BrewPlatform::from_triple("x86_64-pc-windows-msvc"),
            None
        );
    }

    #[test]
    fn test_generate_formula_all_platforms() {
        let mut platforms = BTreeMap::new();
        platforms.insert(
            BrewPlatform::MacosArm,
            PlatformAsset {
                url: "https://example.com/tool-aarch64-apple-darwin-v1.0.0.tar.gz".to_string(),
                sha256: "aaaa".to_string(),
            },
        );
        platforms.insert(
            BrewPlatform::MacosIntel,
            PlatformAsset {
                url: "https://example.com/tool-x86_64-apple-darwin-v1.0.0.tar.gz".to_string(),
                sha256: "bbbb".to_string(),
            },
        );
        platforms.insert(
            BrewPlatform::LinuxArm,
            PlatformAsset {
                url: "https://example.com/tool-aarch64-unknown-linux-gnu-v1.0.0.tar.gz".to_string(),
                sha256: "cccc".to_string(),
            },
        );
        platforms.insert(
            BrewPlatform::LinuxIntel,
            PlatformAsset {
                url: "https://example.com/tool-x86_64-unknown-linux-gnu-v1.0.0.tar.gz".to_string(),
                sha256: "dddd".to_string(),
            },
        );

        let formula = generate_formula(
            "MyTool",
            "A great tool",
            "https://github.com/org/my-tool",
            "1.0.0",
            "my-tool",
            &platforms,
        );

        assert!(formula.contains("class MyTool < Formula"));
        assert!(formula.contains("desc \"A great tool\""));
        assert!(formula.contains("version \"1.0.0\""));
        assert!(formula.contains("on_macos do"));
        assert!(formula.contains("on_linux do"));
        assert!(formula.contains("on_arm do"));
        assert!(formula.contains("on_intel do"));
        assert!(formula.contains("bin.install \"my-tool\""));
        assert!(formula.contains("sha256 \"aaaa\""));
        assert!(formula.contains("sha256 \"dddd\""));
    }

    #[test]
    fn test_generate_formula_macos_only() {
        let mut platforms = BTreeMap::new();
        platforms.insert(
            BrewPlatform::MacosArm,
            PlatformAsset {
                url: "https://example.com/arm.tar.gz".to_string(),
                sha256: "aaaa".to_string(),
            },
        );

        let formula = generate_formula(
            "Tool",
            "desc",
            "https://example.com",
            "1.0.0",
            "tool",
            &platforms,
        );

        assert!(formula.contains("on_macos do"));
        assert!(!formula.contains("on_linux do"));
    }
}
