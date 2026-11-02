//! GitHub `ForgePort` adapter (V5REPOS-9).
//!
//! Reads/writes the D3 intersection against `api.github.com`.

use async_trait::async_trait;
use reqwest::{header, Client, StatusCode};
use serde_json::Value;

use crate::v5::adapters::{
    extract_host, ForgeAuth, ForgeErrorClass, ForgeMetadata, ForgePort, ForgePortError,
    MetadataFields, ProviderVisibility,
};
use crate::v5::config::{ProviderKind, Remote, RepoRef};

const DEFAULT_HOST: &str = "api.github.com";

/// GitHub adapter. Host-pinned to `api.github.com` for v1; GHES is a
/// future extension.
#[derive(Clone, Default)]
pub struct GithubAdapter {
    api_host: String,
}

impl GithubAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_host: DEFAULT_HOST.to_string(),
        }
    }

    async fn token(auth: &ForgeAuth<'_>) -> Result<String, ForgePortError> {
        // V5PARITY-24: try explicit token_ref first; fall back to
        // the org's provider-default. Both paths handled by
        // ForgeAuth::resolve_token.
        let value = auth.resolve_token()?;
        if value.trim().is_empty() {
            return Err(ForgePortError::auth("token is empty"));
        }
        Ok(value)
    }

    fn api_url(&self, repo_ref: &RepoRef) -> String {
        format!(
            "https://{}/repos/{}/{}",
            self.api_host, repo_ref.org, repo_ref.name
        )
    }

    fn build_client() -> Result<Client, ForgePortError> {
        Client::builder()
            .user_agent("hyperforge-v5")
            .build()
            .map_err(|e| ForgePortError::network(format!("client build: {e}")))
    }

    fn auth_headers(token: &str) -> Result<header::HeaderMap, ForgePortError> {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ForgePortError::auth("invalid token for header"))?,
        );
        h.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        h.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        Ok(h)
    }

    fn map_status_error(status: StatusCode, body: &str) -> ForgePortError {
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                // GitHub returns 403 for rate limit with specific header;
                // we detect via body tokens.
                if body.contains("rate limit") || body.contains("API rate limit exceeded") {
                    ForgePortError::rate_limited(format!("github {status}: {body}"))
                } else {
                    ForgePortError::auth(format!("github {status}: {body}"))
                }
            }
            StatusCode::NOT_FOUND => {
                ForgePortError::not_found(format!("github 404: {body}"))
            }
            StatusCode::TOO_MANY_REQUESTS => {
                ForgePortError::rate_limited(format!("github {status}: {body}"))
            }
            _ => ForgePortError::network(format!("github {status}: {body}")),
        }
    }
}

impl GithubAdapter {
    /// V5PARITY-33: shared paginator. `owner_filter` (when present)
    /// keeps only items whose `owner.login` matches — used by the
    /// `/user/repos` path so cross-user calls don't pollute results.
    async fn list_paginated(
        client: &reqwest::Client,
        headers: &reqwest::header::HeaderMap,
        start_url: &str,
        owner_filter: Option<&str>,
    ) -> Result<Vec<crate::v5::adapters::RemoteRepo>, ForgePortError> {
        let mut items: Vec<crate::v5::adapters::RemoteRepo> = Vec::new();
        let mut url_opt = Some(start_url.to_string());
        while let Some(url) = url_opt.take() {
            let resp = client
                .get(&url)
                .headers(headers.clone())
                .send()
                .await
                .map_err(|e| ForgePortError::network(format!("get {url}: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let b = resp.text().await.unwrap_or_default();
                return Err(Self::map_status_error(status, &b));
            }
            let next = resp.headers().get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_next_link);
            let body: serde_json::Value = resp.json().await
                .map_err(|e| ForgePortError::network(format!("parse repos list: {e}")))?;
            if let Some(arr) = body.as_array() {
                for item in arr {
                    if let Some(want) = owner_filter {
                        let got = item.get("owner")
                            .and_then(|o| o.get("login"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        if got != want { continue; }
                    }
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if name.is_empty() { continue; }
                    let url = item.get("clone_url").and_then(|v| v.as_str())
                        .or_else(|| item.get("html_url").and_then(|v| v.as_str()))
                        .unwrap_or("").to_string();
                    items.push(crate::v5::adapters::RemoteRepo {
                        name,
                        url,
                        default_branch: item.get("default_branch").and_then(|v| v.as_str()).map(String::from),
                        description: item.get("description").and_then(|v| v.as_str()).map(String::from),
                        archived: item.get("archived").and_then(|v| v.as_bool()),
                        visibility: item.get("visibility").and_then(|v| v.as_str()).map(String::from),
                    });
                }
            }
            url_opt = next;
        }
        Ok(items)
    }
}

#[async_trait]
impl ForgePort for GithubAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Github
    }

    async fn read_metadata(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        auth: &ForgeAuth<'_>,
    ) -> Result<ForgeMetadata, ForgePortError> {
        let _ = extract_host(remote.url.as_str()); // validate
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);

        let resp = client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("get {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &body));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ForgePortError::network(format!("parse github body: {e}")))?;

        let default_branch = v
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let description = v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let archived = v.get("archived").and_then(Value::as_bool).unwrap_or(false);

        // GitHub tri-state visibility: prefer `visibility` when present,
        // else fall back to the boolean `private`.
        let visibility = if let Some(vis) = v.get("visibility").and_then(Value::as_str) {
            vis.to_string()
        } else if v.get("private").and_then(Value::as_bool).unwrap_or(false) {
            "private".to_string()
        } else {
            "public".to_string()
        };

        Ok(ForgeMetadata {
            default_branch,
            description,
            archived,
            visibility,
        })
    }

    async fn write_metadata(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        fields: &MetadataFields,
        auth: &ForgeAuth<'_>,
    ) -> Result<MetadataFields, ForgePortError> {
        let _ = extract_host(remote.url.as_str());
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);

        let mut body = serde_json::Map::new();
        for (k, v) in fields {
            body.insert(k.as_str().to_string(), v.clone());
        }
        let resp = client
            .patch(&url)
            .headers(headers)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("patch {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &b));
        }
        // Echo the applied fields back.
        Ok(fields.clone())
    }

    // -----------------------------------------------------------------
    // V5PROV-3 lifecycle methods.
    // -----------------------------------------------------------------

    async fn create_repo(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        visibility: ProviderVisibility,
        description: &str,
        auth: &ForgeAuth<'_>,
    ) -> Result<(), ForgePortError> {
        // `internal` is unsupported on github.com. Reject without any
        // API call so callers (and timing-sensitive tests) see the
        // error class immediately.
        if matches!(visibility, ProviderVisibility::Internal) {
            return Err(ForgePortError::unsupported_visibility(
                "github.com does not support visibility 'internal'",
            ));
        }
        let _ = extract_host(remote.url.as_str());
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;

        let private = matches!(visibility, ProviderVisibility::Private);
        let vis_str = match visibility {
            ProviderVisibility::Public => "public",
            ProviderVisibility::Private => "private",
            ProviderVisibility::Internal => "internal", // unreachable after guard above
        };

        // Body is identical for org and user endpoints.
        let mut body = serde_json::Map::new();
        body.insert(
            "name".to_string(),
            Value::String(repo_ref.name.as_str().to_string()),
        );
        body.insert("private".to_string(), Value::Bool(private));
        body.insert("visibility".to_string(), Value::String(vis_str.to_string()));
        if !description.is_empty() {
            body.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
        }

        // Try the org endpoint first; if the owner is actually a user
        // account, GitHub returns 404 on `/orgs/<user>/repos` and we
        // fall back to `/user/repos`.
        let org_url = format!("https://{}/orgs/{}/repos", self.api_host, repo_ref.org);
        let resp = client
            .post(&org_url)
            .headers(headers.clone())
            .json(&Value::Object(body.clone()))
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("post {org_url}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            let b = resp.text().await.unwrap_or_default();
            if b.contains("already exists") || b.contains("name already exists") {
                return Err(ForgePortError::conflict(format!(
                    "github repo '{}/{}' already exists",
                    repo_ref.org, repo_ref.name
                )));
            }
            return Err(ForgePortError::network(format!("github 422: {b}")));
        }
        if status == StatusCode::NOT_FOUND {
            // Possibly owner is a user, not an org — retry against /user/repos.
            let user_url = format!("https://{}/user/repos", self.api_host);
            let resp2 = client
                .post(&user_url)
                .headers(headers)
                .json(&Value::Object(body))
                .send()
                .await
                .map_err(|e| ForgePortError::network(format!("post {user_url}: {e}")))?;
            let s2 = resp2.status();
            if s2.is_success() {
                return Ok(());
            }
            if s2 == StatusCode::UNPROCESSABLE_ENTITY {
                let b = resp2.text().await.unwrap_or_default();
                if b.contains("already exists") || b.contains("name already exists") {
                    return Err(ForgePortError::conflict(format!(
                        "github repo '{}/{}' already exists",
                        repo_ref.org, repo_ref.name
                    )));
                }
                return Err(ForgePortError::network(format!("github 422: {b}")));
            }
            let b = resp2.text().await.unwrap_or_default();
            return Err(Self::map_status_error(s2, &b));
        }
        let b = resp.text().await.unwrap_or_default();
        Err(Self::map_status_error(status, &b))
    }

    async fn delete_repo(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        auth: &ForgeAuth<'_>,
    ) -> Result<(), ForgePortError> {
        let _ = extract_host(remote.url.as_str());
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);

        let resp = client
            .delete(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("delete {url}: {e}")))?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let b = resp.text().await.unwrap_or_default();
        Err(Self::map_status_error(status, &b))
    }

    async fn repo_exists(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        auth: &ForgeAuth<'_>,
    ) -> Result<bool, ForgePortError> {
        let _ = extract_host(remote.url.as_str());
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);

        let resp = client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("get {url}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(true);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let b = resp.text().await.unwrap_or_default();
        Err(Self::map_status_error(status, &b))
    }

    async fn list_repos(
        &self,
        org: &crate::v5::config::OrgName,
        auth: &ForgeAuth<'_>,
    ) -> Result<Vec<crate::v5::adapters::RemoteRepo>, ForgePortError> {
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;

        // 1. Try the org endpoint. Real orgs return here.
        let org_url = format!("https://api.github.com/orgs/{}/repos?per_page=100", org.as_str());
        match Self::list_paginated(&client, &headers, &org_url, None).await {
            Ok(v) if !v.is_empty() => return Ok(v),
            Ok(_) => {} // empty 200 — uncommon; fall through.
            Err(e) if e.class == ForgeErrorClass::NotFound => {} // user account; fall through.
            Err(e) => return Err(e),
        }

        // 2. V5PARITY-33: when the target is the authenticated user,
        // /user/repos?affiliation=owner returns BOTH public and private
        // repos. /users/{name}/repos (step 3) only returns public.
        // Filter by owner.login so listing user A's repos while authed
        // as B doesn't accidentally surface B's repos.
        let me_url = format!(
            "https://api.github.com/user/repos?affiliation=owner&per_page=100",
        );
        if let Ok(v) = Self::list_paginated(&client, &headers, &me_url, Some(org.as_str())).await {
            if !v.is_empty() {
                return Ok(v);
            }
        }

        // 3. Public-only fallback for someone else's user account
        // (or unauthenticated requests).
        let user_url = format!("https://api.github.com/users/{}/repos?per_page=100", org.as_str());
        Self::list_paginated(&client, &headers, &user_url, None).await
    }


    /// Follow GitHub's rename/transfer redirect and report the repo's
    /// CURRENT owner login. `GET /repos/{owner}/{name}` 301-redirects to
    /// the new location after an org transfer or rename; reqwest follows
    /// redirects by default, so the resolved body's `owner.login` is the
    /// canonical owner. We prefer `owner.login`, falling back to the org
    /// segment of `full_name` (`<owner>/<name>`).
    async fn canonical_owner(
        &self,
        remote: &Remote,
        repo_ref: &RepoRef,
        auth: &ForgeAuth<'_>,
    ) -> Result<String, ForgePortError> {
        let _ = extract_host(remote.url.as_str()); // validate
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);

        let resp = client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("get {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &body));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ForgePortError::network(format!("parse github body: {e}")))?;
        if let Some(login) = v
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(Value::as_str)
        {
            return Ok(login.to_string());
        }
        if let Some(full) = v.get("full_name").and_then(Value::as_str) {
            if let Some((owner, _)) = full.split_once('/') {
                return Ok(owner.to_string());
            }
        }
        Err(ForgePortError::network(
            "github repo response missing owner.login/full_name",
        ))
    }

    async fn rename_repo(
        &self,
        _remote: &Remote,
        repo_ref: &RepoRef,
        new_name: &str,
        auth: &ForgeAuth<'_>,
    ) -> Result<(), ForgePortError> {
        let token = Self::token(auth).await?;
        let client = Self::build_client()?;
        let headers = Self::auth_headers(&token)?;
        let url = self.api_url(repo_ref);
        let body = serde_json::json!({"name": new_name});
        let resp = client
            .patch(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| ForgePortError::network(format!("patch {url}: {e}")))?;
        let status = resp.status();
        if status.is_success() { return Ok(()); }
        let b = resp.text().await.unwrap_or_default();
        Err(Self::map_status_error(status, &b))
    }

    /// V5PARITY-36: github has no public import-from-git API surface.
    /// Use codeberg or gitlab as the migration target instead.
    async fn migrate_from(
        &self,
        _source_url: &str,
        _dest_repo_ref: &crate::v5::config::RepoRef,
        _options: &crate::v5::adapters::MigrateOptions,
        _source_auth: Option<&str>,
        _auth: &ForgeAuth<'_>,
    ) -> Result<crate::v5::adapters::RemoteRepo, ForgePortError> {
        Err(ForgePortError::new(
            ForgeErrorClass::UnsupportedField,
            "github does not support migrate_from (no public import-from-git API)",
        ))
    }
}

/// Parse the `rel="next"` URL out of a GitHub Link header.
fn parse_next_link(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim();
        if part.contains(r#"rel="next""#) {
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}
