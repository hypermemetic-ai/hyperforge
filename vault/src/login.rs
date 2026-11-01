//! The owner's WorkOS sign-in, on the client half: PKCE through a loopback
//! redirect, and the refresh that keeps a token fresh.
//!
//! synapse's login cannot be reused: it stores an opaque Menger session
//! (`mgs_…`), which WorkOS's issuer did not sign. So `vault-login` signs the
//! owner in itself, as a public client: no secret exists anywhere on this side.
//!
//! `login.json` (0600) holds the access and refresh tokens. The access token is
//! short-lived and the socket closes when it ends, so [`Login::token`] hands out
//! one with [`MARGIN`] left to run, refreshing when it must. WorkOS's refresh
//! tokens rotate: the new one is on disk before the access token is used.

use anyhow::{anyhow, bail, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::{Zeroize, Zeroizing};

/// A token must have this long left to be handed out: enough to connect and
/// make one call. The socket's end is the transport's to enforce.
const MARGIN: Duration = Duration::from_secs(5);
/// How long the browser has to come back.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

pub const WORKOS_API: &str = "https://api.workos.com";

/// Where the owner signs in. All of it public; the deployment's values live in
/// `config/production.env`, not here.
#[derive(clap::Args, Debug, Clone)]
pub struct WorkosArgs {
    /// The WorkOS client's public id. An `Option` only so that a tool's offline
    /// subcommands, which `subcommand_negates_reqs` frees of it, still parse:
    /// `vault-approve recover` needs no sign-in. Clap requires it everywhere else.
    #[arg(long, env = "VAULT_WORKOS_CLIENT_ID", required = true)]
    pub workos_client_id: Option<String>,
    /// The WorkOS API.
    #[arg(long, env = "VAULT_WORKOS_BASE", default_value = WORKOS_API)]
    pub workos_base: String,
    /// Where `vault-login` keeps the owner's tokens, owner-only.
    #[arg(long, env = "VAULT_LOGIN_FILE")]
    pub login_file: Option<PathBuf>,
}

impl WorkosArgs {
    pub fn login_path(&self) -> anyhow::Result<PathBuf> {
        match &self.login_file {
            Some(path) => Ok(path.clone()),
            None => Ok(PathBuf::from(std::env::var("HOME").context("no HOME: pass --login-file")?).join(".config/vault/login.json")),
        }
    }

    pub fn login(&self) -> anyhow::Result<Login> {
        let client_id = self.workos_client_id.clone().context("no WorkOS client id: pass --workos-client-id")?;
        Ok(Login { client_id, base: self.workos_base.trim_end_matches('/').to_string(), file: self.login_path()?, http: http()? })
    }
}

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
    async fn authenticate(&self, grant: Value) -> anyhow::Result<Tokens> {
        let url = format!("{}/user_management/authenticate", self.base);
        let response = self.http.post(&url).json(&grant).send().await.with_context(|| format!("reaching {url}"))?;
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
        let grant = json!({"client_id": self.client_id, "grant_type": "refresh_token", "refresh_token": tokens.refresh_token});
        let fresh = self.authenticate(grant).await.context("refreshing the sign-in: run vault-login if this persists")?;
        write_tokens(&self.file, &fresh)?;
        Ok(Zeroizing::new(fresh.access_token.clone()))
    }
}

/// `n` random bytes, base64url: a verifier or a state.
fn random() -> anyhow::Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes[..])).context("reading /dev/urandom")?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(&bytes[..])))
}

/// What a login needs beyond [`Login`].
pub struct LoginRequest<'a> {
    pub redirect_uri: &'a str,
    /// `authkit`, or the connection the WorkOS environment signs in with.
    pub provider: &'a str,
}

/// Runs the flow: listens on the redirect, shows the URL (and `open_browser`s
/// it), and stores what the code exchanges for.
pub async fn sign_in(login: &Login, request: LoginRequest<'_>, open_browser: impl FnOnce(&str)) -> anyhow::Result<()> {
    let redirect = Url::parse(request.redirect_uri).context("--redirect-uri is not a URL")?;
    let host = redirect.host_str().context("--redirect-uri has no host")?.to_string();
    if redirect.scheme() != "http" || !matches!(host.as_str(), "127.0.0.1" | "localhost") {
        bail!("--redirect-uri must be http://127.0.0.1:<port>/… or http://localhost:<port>/…: this tool only listens on loopback");
    }
    let port = redirect.port().context("--redirect-uri needs a port: WorkOS matches it exactly")?;
    let listener = TcpListener::bind((host.as_str(), port)).await.with_context(|| format!("listening on {host}:{port}"))?;

    let verifier = random()?;
    let state = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(&format!("{}/user_management/authorize", login.base)).context("the WorkOS base is not a URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &login.client_id)
        .append_pair("redirect_uri", request.redirect_uri)
        .append_pair("provider", request.provider)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    println!("Sign in at:\n{url}");
    std::io::stdout().flush()?;
    open_browser(url.as_str());

    let code = tokio::time::timeout(LOGIN_TIMEOUT, callback(&listener, redirect.path(), &state)).await.context("the browser did not come back")??;
    let grant = json!({"client_id": login.client_id, "grant_type": "authorization_code", "code": *code, "code_verifier": *verifier});
    let tokens = login.authenticate(grant).await?;
    write_tokens(&login.file, &tokens)?;
    println!("signed in: {}", login.file.display());
    Ok(())
}

/// The code from the first request that carries this flow's `state`. Anything
/// else on the port is told no and ignored: another local page can reach it.
async fn callback(listener: &TcpListener, path: &str, state: &str) -> anyhow::Result<Zeroizing<String>> {
    loop {
        let (mut socket, _) = listener.accept().await.context("the redirect listener")?;
        let mut buffer = Zeroizing::new(vec![0u8; 8192]);
        // To the end of the headers, however the request was split up: one
        // read is not one request. A silent connection does not hold the flow.
        let mut n = 0;
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while n < buffer.len() && !buffer[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buffer[n..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(more) => n += more,
                }
            }
        })
        .await;
        let head = Zeroizing::new(String::from_utf8_lossy(&buffer[..n]).into_owned());
        let target = head.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("");
        let parsed = Url::parse(&format!("http://loopback{target}")).ok().filter(|u| u.path() == path);
        let query: Vec<(String, String)> = parsed.iter().flat_map(|u| u.query_pairs().into_owned()).collect();
        let field = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let ours = field("state") == Some(state);
        let (status, body) = match ours {
            true => ("200 OK", "Signed in. You can close this tab."),
            false => ("400 Bad Request", "This is not the sign-in that is waiting."),
        };
        let _ = socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        if !ours {
            continue;
        }
        if let Some(error) = field("error") {
            bail!("WorkOS said: {}", crate::wire::printable(error, 100));
        }
        return Ok(Zeroizing::new(field("code").context("the redirect carried no code")?.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_verifier_is_long_enough_and_never_repeats() {
        let (a, b) = (random().unwrap(), random().unwrap());
        assert!((43..=128).contains(&a.len()), "RFC 7636: {}", a.len());
        assert_ne!(*a, *b);
    }

    #[tokio::test]
    async fn a_redirect_that_is_not_loopback_is_refused_before_anything_listens() {
        let args = WorkosArgs { workos_client_id: Some("c".into()), workos_base: WORKOS_API.into(), login_file: Some("/nonexistent/x".into()) };
        for bad in ["https://127.0.0.1:1/cb", "http://example.com:1/cb", "http://127.0.0.1/cb", "not a url"] {
            let answer = sign_in(&args.login().unwrap(), LoginRequest { redirect_uri: bad, provider: "authkit" }, |_| {}).await;
            assert!(answer.is_err(), "{bad}");
        }
    }
}
