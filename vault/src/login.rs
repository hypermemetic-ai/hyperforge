//! The owner's WorkOS sign-in, on the client half: the device flow, and the
//! refresh that keeps a token fresh.
//!
//! synapse's login cannot be reused: it stores an opaque Menger session
//! (`mgs_…`), which WorkOS's issuer did not sign. So `vault-login` signs the
//! owner in itself, as a public client: no secret exists anywhere on this side,
//! and nothing listens on this machine. [`sign_in`] asks WorkOS for a code, the
//! owner approves it in a browser, on any device, and this side polls.
//!
//! What comes back is checked before it is stored: WorkOS's signature, issuer
//! and client through `OidcValidator`, the check the server runs at its door,
//! and then that it is the owner's.
//!
//! `login.json` (0600) holds the access and refresh tokens. The access token is
//! short-lived and the socket closes when it ends, so [`Login::token`] hands out
//! one with [`MARGIN`] left to run, refreshing when it must. WorkOS's refresh
//! tokens rotate: the new one is on disk before the access token is used.
//!
//! This is shared code: it takes its settings as values, and leaves the
//! printing, the browser and the command line to `vault-login`.

use anyhow::{anyhow, bail, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use plexus_auth_core::capabilities::IssuerUrl;
use plexus_auth_core::oidc::{Audience, AudienceClaim, OidcConfig, OidcError, OidcValidator};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

/// A token must have this long left to be handed out: enough to connect and
/// make one call. The socket's end is the transport's to enforce.
const MARGIN: Duration = Duration::from_secs(5);
/// RFC 8628 3.2 and 3.5, for an answer that gives no `interval`, and for what
/// `slow_down` adds. WorkOS's docs show 5 and ask for "more".
const DEFAULT_INTERVAL: u64 = 5;
const SLOW_DOWN: Duration = Duration::from_secs(5);
/// The sooner of this and the server's `expires_in` ends the wait. WorkOS shows
/// 300: a server saying a year does not get to keep this tool waiting a year.
const LONGEST_WAIT: u64 = 1800;
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub const WORKOS_API: &str = "https://api.workos.com";

fn http() -> anyhow::Result<reqwest::Client> {
    // A token endpoint has no business redirecting a token anywhere.
    reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().context("the HTTP client")
}

#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
}

impl Drop for Tokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// When a token says it ends. Unverified, and only used to decide whether to
/// refresh: the server is what checks it.
fn expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims.get("exp")?.as_i64()
}

fn write_tokens(path: &Path, tokens: &Tokens) -> anyhow::Result<()> {
    let parent = path.parent().context("the login file has no directory")?;
    std::fs::DirBuilder::new().recursive(true).create(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("tmp");
    let mut bytes = Zeroizing::new(serde_json::to_vec(tokens)?);
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    bytes.zeroize();
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The owner's stored sign-in.
pub struct Login {
    client_id: String,
    base: String,
    file: PathBuf,
    http: reqwest::Client,
}

impl Login {
    /// `client_id` and `base` are public; `file` is where the tokens live.
    pub fn new(client_id: String, base: &str, file: PathBuf) -> anyhow::Result<Login> {
        Ok(Login { client_id, base: base.trim_end_matches('/').to_string(), file, http: http()? })
    }

    /// A form, as WorkOS's CLI Auth docs show it and as [`Login::poll`] sends.
    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<Tokens> {
        let url = format!("{}/user_management/authenticate", self.base);
        let form = [("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", self.client_id.as_str())];
        let response = self.http.post(&url).form(&form).send().await.with_context(|| format!("reaching {url}"))?;
        let status = response.status();
        if !status.is_success() {
            // The status and nothing else: an error body may echo the grant.
            bail!("WorkOS refused the sign-in ({status})");
        }
        response.json().await.context("reading WorkOS's answer")
    }

    /// A token good for now, refreshed if need be.
    pub async fn token(&self) -> anyhow::Result<Zeroizing<String>> {
        let mut text = Zeroizing::new(String::new());
        let read = std::fs::File::open(&self.file).and_then(|mut f| f.read_to_string(&mut text));
        read.with_context(|| format!("no sign-in at {}: run vault-login", self.file.display()))?;
        let tokens: Tokens = serde_json::from_str(&text).map_err(|_| anyhow!("{} is not a sign-in: run vault-login", self.file.display()))?;
        let good_until = now() + MARGIN.as_secs() as i64;
        if expiry(&tokens.access_token).is_some_and(|exp| exp > good_until) {
            return Ok(Zeroizing::new(tokens.access_token.clone()));
        }
        let fresh = self.refresh(&tokens.refresh_token).await.context("refreshing the sign-in: run vault-login if this persists")?;
        write_tokens(&self.file, &fresh)?;
        Ok(Zeroizing::new(fresh.access_token.clone()))
    }
}

/// What WorkOS's device endpoint answers. `verification_uri_complete` is left
/// out on purpose: the one thing this tool opens is the page where the owner
/// types the code they can see here.
#[derive(Deserialize)]
struct Authorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

impl Drop for Authorization {
    fn drop(&mut self) {
        self.device_code.zeroize();
    }
}

/// What the owner has to be told, to be shown however the caller shows things.
/// Everything here has been through [`crate::wire::printable`].
pub struct Prompt {
    pub verification_uri: String,
    pub user_code: String,
    /// The page, if it is on a host this tool may open by itself.
    pub open: Option<String>,
}

impl Prompt {
    /// For a terminal.
    pub fn instructions(&self) -> String {
        let warning = match self.open {
            Some(_) => "",
            None => "That is not a WorkOS address, so it was not opened. Check it before you use it.\n",
        };
        format!(
            "Sign in at:\n{}\n{warning}and enter this code:\n{}\nEnter a code only if you started this sign-in yourself, just now. If a person, page or message gave you the code, stop.",
            self.verification_uri, self.user_code
        )
    }
}

/// `uri`, normalised, if it is plain https on the standard port of a WorkOS or
/// AuthKit host. Nothing else is opened: a custom AuthKit domain is printed, and
/// the owner judges it.
fn openable(uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return None;
    }
    let host = url.domain()?;
    ["workos.com", "authkit.app"]
        .iter()
        .any(|root| host == *root || host.strip_suffix(root).is_some_and(|rest| rest.ends_with('.')))
        .then(|| url.to_string())
}

/// What a poll that did not succeed says.
#[derive(Deserialize)]
struct PollError {
    error: String,
}

impl Login {
    /// The check the server's door makes: `OidcAdapter::workos_config`, which
    /// is behind the server feature with the rest of the adapters.
    fn oidc_config(&self) -> anyhow::Result<OidcConfig> {
        let issuer = format!("{}/user_management/{}", self.base, self.client_id);
        let url = Url::parse(&issuer).context("the WorkOS base is not a URL")?;
        let issuer = IssuerUrl::try_new(url).map_err(|_| anyhow!("the WorkOS base must be https"))?;
        let audience = Audience::try_new(self.client_id.as_str()).map_err(|_| anyhow!("the WorkOS client id is not printable"))?;
        Ok(OidcConfig::new(issuer, audience).with_audience_claim(AudienceClaim::ClientId))
    }

    /// Asks for a code. No secret and no redirect: the client id is the request.
    async fn authorize(&self) -> anyhow::Result<Authorization> {
        let url = format!("{}/user_management/authorize/device", self.base);
        let response = self.http.post(&url).form(&[("client_id", self.client_id.as_str())]).send().await.with_context(|| format!("reaching {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("WorkOS refused to start a sign-in ({status})");
        }
        let body = response.bytes().await.map_err(|_| anyhow!("reading WorkOS's answer"))?;
        let authorization: Authorization = serde_json::from_slice(&body).map_err(|_| anyhow!("WorkOS's answer is not a device authorization"))?;
        if authorization.device_code.is_empty() || authorization.user_code.is_empty() || authorization.verification_uri.is_empty() || authorization.expires_in == 0 {
            bail!("WorkOS's answer is not a device authorization");
        }
        Ok(authorization)
    }

    /// Polls until WorkOS answers for good or `deadline` passes, at `interval`
    /// and never sooner. Only the two answers that mean "keep waiting" keep it
    /// waiting. Anything WorkOS says beyond its status stays out of the error.
    async fn poll(&self, authorization: &Authorization, deadline: Instant) -> anyhow::Result<Tokens> {
        let url = format!("{}/user_management/authenticate", self.base);
        let form = [("grant_type", DEVICE_GRANT), ("device_code", authorization.device_code.as_str()), ("client_id", self.client_id.as_str())];
        let mut interval = Duration::from_secs(authorization.interval.unwrap_or(DEFAULT_INTERVAL).max(1));
        loop {
            if Instant::now() + interval >= deadline {
                bail!("the code expired before the sign-in finished: run vault-login again");
            }
            tokio::time::sleep(interval).await;
            let exchange = async {
                let response = self.http.post(&url).form(&form).send().await?;
                let status = response.status();
                Ok::<_, reqwest::Error>((status, Zeroizing::new(response.bytes().await?.to_vec())))
            };
            let (status, body) = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), exchange)
                .await
                .map_err(|_| anyhow!("the code expired before the sign-in finished: run vault-login again"))?
                .with_context(|| format!("reaching {url}"))?;
            if status.is_success() {
                let tokens: Tokens = serde_json::from_slice(&body).map_err(|_| anyhow!("WorkOS's answer is not a sign-in"))?;
                if tokens.refresh_token.is_empty() {
                    bail!("WorkOS's answer is not a sign-in");
                }
                return Ok(tokens);
            }
            match serde_json::from_slice::<PollError>(&body).map(|e| e.error).as_deref() {
                Ok("authorization_pending") => {}
                Ok("slow_down") => interval += SLOW_DOWN,
                Ok("access_denied") => bail!("the sign-in was denied"),
                Ok("expired_token") => bail!("the code expired before the sign-in finished: run vault-login again"),
                _ => bail!("WorkOS refused the sign-in ({status})"),
            }
        }
    }
}

/// Why a token was not taken. Fixed words: what the validator says can quote
/// what it was given.
fn refusal(error: OidcError) -> anyhow::Error {
    let why = match error {
        OidcError::Discovery(_) | OidcError::JwksUnavailable(_) => "WorkOS's keys could not be read",
        OidcError::WrongIssuer => "it names another issuer",
        OidcError::WrongAudience => "it is for another client",
        OidcError::Expired => "it has expired",
        OidcError::InvalidSignature | OidcError::UnknownKeyId(_) => "WorkOS did not sign it",
        _ => "it is not a token this tool can read",
    };
    anyhow!("the sign-in was refused and nothing was stored: {why}")
}

/// Runs the flow: asks for a code, has `show` tell the owner, waits for the
/// answer, and stores it if it is a token for this client and for `owner`, the
/// WorkOS user id. Otherwise nothing changes on disk. Any sign-in already there
/// stands.
pub async fn sign_in(login: &Login, owner: &str, show: impl FnOnce(&Prompt)) -> anyhow::Result<()> {
    if owner.is_empty() {
        bail!("no owner: set VAULT_OWNER_USER_ID");
    }
    // Before anything is asked of anyone, so a bad setting is not a wasted code.
    let validator = OidcValidator::new(login.oidc_config()?);
    let started = Instant::now();
    let authorization = login.authorize().await?;
    show(&Prompt {
        verification_uri: crate::wire::printable(&authorization.verification_uri, 300),
        user_code: crate::wire::printable(&authorization.user_code, 64),
        open: openable(&authorization.verification_uri),
    });
    let deadline = started + Duration::from_secs(authorization.expires_in.min(LONGEST_WAIT));
    let tokens = login.poll(&authorization, deadline).await?;
    let verified = validator.validate_token(&tokens.access_token).await.map_err(refusal)?;
    if verified.user().user_id() != owner {
        // Say nothing of who: Menger's client is shared.
        bail!("the sign-in was refused and nothing was stored: that account is not the owner");
    }
    write_tokens(&login.file, &tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token_ending(exp: i64) -> String {
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(json!({"exp": exp}).to_string()))
    }

    #[test]
    fn expiry_reads_exp_and_nothing_else_parses() {
        assert_eq!(expiry(&token_ending(1234)), Some(1234));
        for bad in ["", "no-dots", "a.b", "a.%%%.c", "mgs_0123"] {
            assert_eq!(expiry(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_plain_https_on_workos_and_authkit_hosts_may_open() {
        for good in ["https://workos.com/device", "https://a.workos.com/device", "https://authkit.app/x", "https://a.b.authkit.app/device?x=1"] {
            assert_eq!(openable(good).as_deref(), Some(good));
        }
        for bad in ["http://a.authkit.app/", "https://evilauthkit.app/", "https://authkit.app.evil.example/", "https://a.authkit.app@evil.example/", "https://evil.example\\@a.authkit.app/", "https://x:y@workos.com/", "https://workos.com:444/", "https://192.0.2.1/", "javascript:alert(1)", "a.authkit.app", ""] {
            assert_eq!(openable(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_issuer_off_https_is_refused_before_anything_is_asked() {
        let login = Login::new("c".into(), "http://example.com", "/nonexistent/x".into()).unwrap();
        assert!(login.oidc_config().is_err());
        assert!(Login::new("c".into(), WORKOS_API, "/nonexistent/x".into()).unwrap().oidc_config().is_ok());
    }

    /// The client must check what the server's door will check.
    #[cfg(feature = "server")]
    #[test]
    fn the_check_here_is_the_one_at_the_door() {
        use plexus_auth_core::adapters::OidcAdapter;
        for base in [WORKOS_API, "http://localhost:1"] {
            let login = Login::new("client_x".into(), base, "/nonexistent/x".into()).unwrap();
            assert_eq!(login.oidc_config().unwrap(), OidcAdapter::workos_config("client_x", Some(base)).unwrap());
        }
    }
}
