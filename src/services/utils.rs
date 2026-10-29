//! Shared helpers used by both `WorkspaceService` and `BuildService`.

use std::sync::Arc;

use crate::adapters::{CodebergAdapter, ForgePort, GitHubAdapter, GitLabAdapter};
use crate::auth::YamlAuthProvider;
use crate::config::HyperforgeConfig;
use crate::service::HyperforgeEvent;
use crate::types::{Forge, OwnerType};

/// Create a forge adapter from a forge name string.
pub(crate) fn make_adapter(
    forge: &str,
    org: &str,
    owner_type: Option<OwnerType>,
) -> Result<Arc<dyn ForgePort>, String> {
    let auth = YamlAuthProvider::new()
        .map_err(|e| format!("Failed to create auth provider: {e}"))?;
    let auth = Arc::new(auth);
    let target_forge = HyperforgeConfig::parse_forge(forge)
        .ok_or_else(|| format!("Invalid forge: {forge}. Must be github, codeberg, or gitlab"))?;
    let adapter: Arc<dyn ForgePort> = match target_forge {
        Forge::GitHub => {
            let a = GitHubAdapter::new(auth, org)
                .map_err(|e| format!("Failed to create GitHub adapter: {e}"))?;
            Arc::new(match owner_type {
                Some(ot) => a.with_owner_type(ot),
                None => a,
            })
        }
        Forge::Codeberg => {
            let a = CodebergAdapter::new(auth, org)
                .map_err(|e| format!("Failed to create Codeberg adapter: {e}"))?;
            Arc::new(match owner_type {
                Some(ot) => a.with_owner_type(ot),
                None => a,
            })
        }
        Forge::GitLab => {
            let a = GitLabAdapter::new(auth, org)
                .map_err(|e| format!("Failed to create GitLab adapter: {e}"))?;
            Arc::new(match owner_type {
                Some(ot) => a.with_owner_type(ot),
                None => a,
            })
        }
    };
    Ok(adapter)
}

/// Simple glob matching for repo name filtering.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if pattern.contains('*') {
        // Simple prefix*suffix matching
        let parts: Vec<&str> = pattern.splitn(2, '*').collect();
        if parts.len() == 2 {
            return name.starts_with(parts[0]) && name.ends_with(parts[1]);
        }
    }
    name == pattern
}

/// Glob-based include/exclude filter for repo names.
/// Exclude always takes priority over include.
pub struct RepoFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl RepoFilter {
    pub fn new(include: Option<Vec<String>>, exclude: Option<Vec<String>>) -> Self {
        Self {
            include: include.unwrap_or_default(),
            exclude: exclude.unwrap_or_default(),
        }
    }

    /// Returns true if the name passes the filter.
    /// - If excludes match, always false (exclude wins)
    /// - If includes are non-empty, name must match at least one
    /// - If both are empty, everything passes
    pub fn matches(&self, name: &str) -> bool {
        if self.exclude.iter().any(|pat| glob_match(pat, name)) {
            return false;
        }
        if self.include.is_empty() {
            return true;
        }
        self.include.iter().any(|pat| glob_match(pat, name))
    }

    pub const fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// Build a default `WorkspaceSummary` event with all optional fields set to None.
pub(crate) const fn workspace_summary(
    ctx: &crate::commands::workspace::WorkspaceContext,
) -> HyperforgeEvent {
    HyperforgeEvent::WorkspaceSummary {
        total_repos: ctx.repos.len() + ctx.unconfigured_repos.len(),
        configured_repos: ctx.repos.len(),
        unconfigured_repos: ctx.unconfigured_repos.len(),
        clean_repos: None,
        dirty_repos: None,
        wrong_branch_repos: None,
        push_success: None,
        push_failed: None,
        validation_passed: None,
    }
}

/// Return a prefix string for dry-run messages.
pub const fn dry_prefix(is_dry_run: bool) -> &'static str {
    if is_dry_run {
        "[DRY RUN] "
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_matches_everything() {
        let f = RepoFilter::new(None, None);
        assert!(f.matches("anything"));
        assert!(f.matches("foo-bar"));
        assert!(f.is_empty());
    }

    #[test]
    fn include_only() {
        let f = RepoFilter::new(Some(vec!["hyper*".into(), "plexus*".into()]), None);
        assert!(f.matches("hyperforge"));
        assert!(f.matches("plexus-core"));
        assert!(!f.matches("synapse"));
        assert!(!f.is_empty());
    }

    #[test]
    fn exclude_only() {
        let f = RepoFilter::new(None, Some(vec!["*-test".into(), "legacy*".into()]));
        assert!(!f.matches("core-test"));
        assert!(!f.matches("legacy-utils"));
        assert!(f.matches("hyperforge"));
        assert!(f.matches("plexus-core"));
    }

    #[test]
    fn exclude_wins_on_overlap() {
        let f = RepoFilter::new(
            Some(vec!["hyper*".into()]),
            Some(vec!["*-deprecated".into()]),
        );
        assert!(f.matches("hyperforge"));
        assert!(!f.matches("hyper-deprecated"));
        assert!(!f.matches("synapse"));
    }

    #[test]
    fn multiple_patterns() {
        let f = RepoFilter::new(
            Some(vec!["core*".into(), "lib*".into()]),
            Some(vec!["core-deprecated".into()]),
        );
        assert!(f.matches("core-utils"));
        assert!(f.matches("lib-common"));
        assert!(!f.matches("core-deprecated"));
        assert!(!f.matches("synapse"));
    }
}
