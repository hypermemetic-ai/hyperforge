//! Static credential registry for all known secrets hyperforge manages.
//!
//! Each credential has a spec describing its key pattern, kind, validation
//! method, and which distribution channels / forges require it.

use crate::types::config::DistChannel;
use crate::types::Forge;

/// Kind of credential
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKind {
    /// Simple bearer/PAT token
    BearerToken,
    /// Classic PAT requiring specific OAuth scopes
    ClassicPat {
        required_scopes: &'static [&'static str],
    },
    /// Username + password pair (two secrets — each half is its own spec)
    UsernamePassword,
    /// Simple API key
    ApiKey,
}

/// How to validate a credential
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationMethod {
    /// GET an endpoint with auth, expect 200
    HttpGet {
        url_pattern: &'static str,
        auth_scheme: &'static str,
    },
    /// GET GitHub /user, parse X-OAuth-Scopes header
    GitHubScopes {
        required: &'static [&'static str],
    },
    /// Just check the secret exists in the store
    ExistsOnly,
}

/// A known credential specification
#[derive(Debug, Clone)]
pub struct CredentialSpec {
    /// Key path pattern in secrets store (e.g. "github/{org}/token")
    pub key_pattern: &'static str,
    /// Human-readable name
    pub display_name: &'static str,
    /// What kind of credential
    pub kind: CredentialKind,
    /// URL where user creates this credential
    pub setup_url: &'static str,
    /// Setup instructions
    pub instructions: &'static str,
    /// How to validate
    pub validation: ValidationMethod,
    /// Which distribution channels / roles need this credential
    pub required_by: &'static [&'static str],
}

/// A resolved credential with {org} substituted
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub spec: &'static CredentialSpec,
    pub key_path: String,
    pub org: String,
}

// ---------------------------------------------------------------------------
// Static registry
// ---------------------------------------------------------------------------

pub const CREDENTIALS: &[CredentialSpec] = &[
    // ── GitHub ────────────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "github/{org}/token",
        display_name: "GitHub Personal Access Token",
        kind: CredentialKind::BearerToken,
        setup_url: "https://github.com/settings/tokens/new",
        instructions: "Create a fine-grained PAT with 'Contents: Read and write' permission for the org",
        validation: ValidationMethod::HttpGet {
            url_pattern: "https://api.github.com/user",
            auth_scheme: "Bearer",
        },
        required_by: &["forge-release", "sync"],
    },
    CredentialSpec {
        key_pattern: "github/{org}/packages_token",
        display_name: "GitHub Classic PAT (packages)",
        kind: CredentialKind::ClassicPat {
            required_scopes: &["read:packages", "write:packages"],
        },
        setup_url: "https://github.com/settings/tokens/new?scopes=read:packages,write:packages",
        instructions: "Create a CLASSIC token (not fine-grained) with read:packages and write:packages scopes",
        validation: ValidationMethod::GitHubScopes {
            required: &["read:packages", "write:packages"],
        },
        required_by: &["ghcr"],
    },
    // ── Codeberg ─────────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "codeberg/{org}/token",
        display_name: "Codeberg Personal Access Token",
        kind: CredentialKind::BearerToken,
        setup_url: "https://codeberg.org/user/settings/applications",
        instructions: "Create a token with repository read/write permissions",
        validation: ValidationMethod::HttpGet {
            url_pattern: "https://codeberg.org/api/v1/user",
            auth_scheme: "Bearer",
        },
        required_by: &["forge-release", "sync"],
    },
    // ── GitLab ───────────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "gitlab/{org}/token",
        display_name: "GitLab Personal Access Token",
        kind: CredentialKind::BearerToken,
        setup_url: "https://gitlab.com/-/user_settings/personal_access_tokens",
        instructions: "Create a token with api scope",
        validation: ValidationMethod::HttpGet {
            url_pattern: "https://gitlab.com/api/v4/user",
            auth_scheme: "Bearer",
        },
        required_by: &["forge-release", "sync"],
    },
    // ── crates.io ────────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "crates-io/token",
        display_name: "crates.io API Token",
        kind: CredentialKind::ApiKey,
        setup_url: "https://crates.io/settings/tokens",
        instructions: "Create an API token with publish-update scope",
        validation: ValidationMethod::HttpGet {
            url_pattern: "https://crates.io/api/v1/me",
            auth_scheme: "Bearer",
        },
        required_by: &["crates-io"],
    },
    // ── Hackage ──────────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "hackage/username",
        display_name: "Hackage Username",
        kind: CredentialKind::UsernamePassword,
        setup_url: "https://hackage.haskell.org/users/register",
        instructions: "Your Hackage account username",
        validation: ValidationMethod::ExistsOnly,
        required_by: &["hackage"],
    },
    CredentialSpec {
        key_pattern: "hackage/password",
        display_name: "Hackage Password",
        kind: CredentialKind::UsernamePassword,
        setup_url: "https://hackage.haskell.org/users/register",
        instructions: "Your Hackage account password",
        validation: ValidationMethod::ExistsOnly,
        required_by: &["hackage"],
    },
    // ── npm (future) ─────────────────────────────────────────────────
    CredentialSpec {
        key_pattern: "npm/token",
        display_name: "npm Access Token",
        kind: CredentialKind::ApiKey,
        setup_url: "https://www.npmjs.com/settings/tokens",
        instructions: "Create an automation token with publish permission",
        validation: ValidationMethod::ExistsOnly,
        required_by: &["npm"],
    },
    // ── Cachix / Nix (future) ────────────────────────────────────────
    CredentialSpec {
        key_pattern: "cachix/{org}/token",
        display_name: "Cachix Auth Token",
        kind: CredentialKind::ApiKey,
        setup_url: "https://app.cachix.org",
        instructions: "Create an auth token for your Cachix cache",
        validation: ValidationMethod::ExistsOnly,
        required_by: &["nix"],
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve `{org}` placeholders in a key pattern.
#[allow(clippy::literal_string_with_formatting_args)]
pub fn resolve_key_path(pattern: &str, org: &str) -> String {
    // Literal template: not a format!() macro; `{org}` is a placeholder this
    // function substitutes.
    pattern.replace("{org}", org)
}

/// Get all credentials needed for a set of distribution channels + org.
pub fn credentials_for_channels(
    channels: &[DistChannel],
    org: &str,
) -> Vec<ResolvedCredential> {
    let channel_tags: Vec<String> = channels.iter().map(std::string::ToString::to_string).collect();

    let mut out = Vec::new();
    for spec in CREDENTIALS {
        let needed = spec
            .required_by
            .iter()
            .any(|tag| channel_tags.iter().any(|ct| ct == *tag));
        if needed {
            out.push(ResolvedCredential {
                spec,
                key_path: resolve_key_path(spec.key_pattern, org),
                org: org.to_string(),
            });
        }
    }
    out
}

/// Get all credentials needed for a forge + org.
///
/// Returns the primary forge token (not packages/GHCR tokens — those are
/// channel-driven via `credentials_for_channels`).
pub fn credentials_for_forge(forge: &Forge, org: &str) -> Vec<ResolvedCredential> {
    let prefix = match forge {
        Forge::GitHub => "github/",
        Forge::Codeberg => "codeberg/",
        Forge::GitLab => "gitlab/",
    };

    // The primary token pattern for each forge is `<forge>/{org}/token`.
    // The `{{org}}` escape yields a literal `{org}` placeholder for the
    // credentials catalogue to compare against — not a format arg.
    #[allow(clippy::literal_string_with_formatting_args)]
    let primary_pattern = format!("{prefix}{{org}}/token");

    CREDENTIALS
        .iter()
        .filter(|spec| spec.key_pattern == primary_pattern)
        .map(|spec| ResolvedCredential {
            spec,
            key_path: resolve_key_path(spec.key_pattern, org),
            org: org.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Pre-flight auth checking
// ---------------------------------------------------------------------------

use crate::auth_service::storage::YamlStorage;
use crate::auth_service::types::SecretPath;
use crate::service::HyperforgeEvent;
use std::collections::HashSet;

/// Result of a pre-flight credential check for a single credential.
#[derive(Debug, Clone)]
pub struct PreflightFailure {
    pub key_path: String,
    pub display_name: &'static str,
    pub needed_by: String,
    pub setup_url: &'static str,
    pub fix_command: String,
}

/// Run pre-flight auth checks for a set of forges and dist channels.
/// Returns error events for any missing credentials.
/// Empty vec = all clear, proceed.
///
/// This only checks existence in the secrets store (no HTTP validation).
/// Full validation is for `auth check`.
pub async fn preflight_check(
    forges: &[String],
    channels: &[DistChannel],
    org: &str,
    _auth: &dyn super::AuthProvider,
) -> Vec<HyperforgeEvent> {
    // Initialize storage
    let storage = match YamlStorage::default_location() {
        Ok(s) => s,
        Err(e) => {
            return vec![HyperforgeEvent::Error {
                message: format!("Pre-flight: failed to initialize secrets storage: {e}"),
            }];
        }
    };
    if let Err(e) = storage.load().await {
        return vec![HyperforgeEvent::Error {
            message: format!("Pre-flight: failed to load secrets: {e}"),
        }];
    }

    // Collect all required credentials, deduplicating by key_path
    let mut seen_keys = HashSet::new();
    let mut all_creds: Vec<ResolvedCredential> = Vec::new();

    // Forge credentials (primary tokens for API access)
    for forge_name in forges {
        if let Some(forge_enum) = crate::config::HyperforgeConfig::parse_forge(forge_name) {
            for cred in credentials_for_forge(&forge_enum, org) {
                if seen_keys.insert(cred.key_path.clone()) {
                    all_creds.push(cred);
                }
            }
        }
    }

    // Channel credentials (crates-io token, hackage creds, etc.)
    if !channels.is_empty() {
        for cred in credentials_for_channels(channels, org) {
            if seen_keys.insert(cred.key_path.clone()) {
                all_creds.push(cred);
            }
        }
    }

    if all_creds.is_empty() {
        return Vec::new();
    }

    // Check existence of each credential
    let mut failures: Vec<PreflightFailure> = Vec::new();

    for cred in &all_creds {
        let secret_path = SecretPath::new(&cred.key_path);
        let exists = match storage.get(&secret_path) {
            Ok(secret) => !secret.value.is_empty(),
            Err(_) => false,
        };

        if !exists {
            let needed_by = cred
                .spec
                .required_by
                .join(", ");
            let fix_command = format!(
                "synapse -P 44105 secrets auth set_secret --secret_key \"{}\" --value \"$(pbpaste)\"",
                cred.key_path,
            );
            failures.push(PreflightFailure {
                key_path: cred.key_path.clone(),
                display_name: cred.spec.display_name,
                needed_by,
                setup_url: cred.spec.setup_url,
                fix_command,
            });
        }
    }

    if failures.is_empty() {
        return Vec::new();
    }

    // Build error events
    let mut events = Vec::new();

    let mut detail_lines = String::from("Pre-flight auth check failed:\n");
    for f in &failures {
        detail_lines.push_str(&format!(
            "\n  ✗ {} — MISSING\n    Needed for: {}\n    Fix: {}\n    Create at: {}\n",
            f.key_path, f.needed_by, f.fix_command, f.setup_url,
        ));
    }
    detail_lines.push_str(&format!(
        "\nAborting. Run `auth setup --org {org}` to configure missing credentials.",
    ));

    events.push(HyperforgeEvent::Error {
        message: detail_lines,
    });

    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_key_path_substitutes_org() {
        assert_eq!(
            resolve_key_path("github/{org}/token", "myorg"),
            "github/myorg/token"
        );
        assert_eq!(
            resolve_key_path("crates-io/token", "myorg"),
            "crates-io/token"
        );
    }

    #[test]
    fn credentials_for_forge_release_returns_github_token() {
        let creds = credentials_for_channels(&[DistChannel::ForgeRelease], "acme");
        let keys: Vec<&str> = creds.iter().map(|c| c.key_path.as_str()).collect();
        assert!(keys.contains(&"github/acme/token"), "expected github token, got {keys:?}");
        assert!(keys.contains(&"codeberg/acme/token"));
        assert!(keys.contains(&"gitlab/acme/token"));
    }

    #[test]
    fn credentials_for_ghcr_returns_packages_token() {
        let creds = credentials_for_channels(&[DistChannel::Ghcr], "acme");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].key_path, "github/acme/packages_token");
    }

    #[test]
    fn credentials_for_crates_io() {
        let creds = credentials_for_channels(&[DistChannel::CratesIo], "acme");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].key_path, "crates-io/token");
    }

    #[test]
    fn credentials_for_hackage_returns_both() {
        let creds = credentials_for_channels(&[DistChannel::Hackage], "acme");
        let keys: Vec<&str> = creds.iter().map(|c| c.key_path.as_str()).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"hackage/username"));
        assert!(keys.contains(&"hackage/password"));
    }

    #[test]
    fn credentials_for_forge_github() {
        let creds = credentials_for_forge(&Forge::GitHub, "myorg");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].key_path, "github/myorg/token");
    }

    #[test]
    fn credentials_for_forge_codeberg() {
        let creds = credentials_for_forge(&Forge::Codeberg, "myorg");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].key_path, "codeberg/myorg/token");
    }

    #[test]
    fn credentials_for_forge_gitlab() {
        let creds = credentials_for_forge(&Forge::GitLab, "myorg");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].key_path, "gitlab/myorg/token");
    }

    #[test]
    fn no_credentials_for_brew_channel() {
        // Brew doesn't require any secret credentials
        let creds = credentials_for_channels(&[DistChannel::Brew], "acme");
        assert!(creds.is_empty());
    }

    #[test]
    fn multiple_channels_dedup() {
        // forge-release + crates-io should return forge tokens + crates token
        let creds = credentials_for_channels(
            &[DistChannel::ForgeRelease, DistChannel::CratesIo],
            "acme",
        );
        let keys: Vec<&str> = creds.iter().map(|c| c.key_path.as_str()).collect();
        assert!(keys.contains(&"github/acme/token"));
        assert!(keys.contains(&"crates-io/token"));
    }
}
