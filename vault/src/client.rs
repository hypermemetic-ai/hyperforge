//! What the client tools share: the RPC, and the caller's side of a request.

use crate::wire::*;
use anyhow::{anyhow, bail, Context};
use futures::StreamExt;
use plexus_client::{decode, ClientEvent, EventStream, MethodRef, Transport, WsTransport};
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// A connection to the vault.
pub struct Rpc {
    transport: WsTransport,
}

impl Rpc {
    /// A connection presenting `credential` on the upgrade: the owner's
    /// WorkOS token ([`crate::login`]) or a machine's. There is no other kind:
    /// the server refuses a socket that presents nothing.
    pub async fn connect(url: &str, credential: &str) -> anyhow::Result<Rpc> {
        let headers = [("Authorization".to_string(), format!("Bearer {credential}"))];
        let transport = WsTransport::connect_with_headers(url, SERVICE, &headers).await.map_err(|e| anyhow!("{e}"))?;
        Ok(Rpc { transport })
    }

    pub fn stream(&self, method: &'static str, params: Value) -> EventStream<'_, VaultEvent> {
        decode::<VaultEvent>(self.transport.call(MethodRef::new(NAMESPACE, method), params))
    }

    /// A call answered by one event; a refusal is an `Err`.
    pub async fn call(&self, method: &'static str, params: Value) -> anyhow::Result<VaultEvent> {
        let mut events = self.stream(method, params);
        loop {
            match events.next().await {
                None => bail!("vault.{method} answered nothing"),
                Some(Err(e)) => bail!("vault.{method}: {e}"),
                Some(Ok(ClientEvent::Data(VaultEvent::Error { message }))) => bail!("{}", printable(&message, 400)),
                Some(Ok(ClientEvent::Data(event))) => return Ok(event),
                Some(Ok(_)) => {}
            }
        }
    }
}

/// A machine's credential, from its file. Empty is an error, not an anonymous
/// caller.
pub fn read_credential(path: &Path) -> anyhow::Result<Zeroizing<String>> {
    let text = Zeroizing::new(std::fs::read_to_string(path).with_context(|| format!("reading the credential {}", path.display()))?);
    let credential = Zeroizing::new(text.trim().to_string());
    if credential.is_empty() {
        bail!("the credential {} is empty", path.display());
    }
    Ok(credential)
}

/// Who is asking, as the caller tells it: the owner sees this, unverified.
pub fn default_caller() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let mut name = [0u8; 256];
    // SAFETY: the buffer is writable for its stated length.
    let ok = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len() - 1) } == 0;
    let end = name.iter().position(|b| *b == 0).unwrap_or(0);
    let host = if ok { String::from_utf8_lossy(&name[..end]).into_owned() } else { "unknown".into() };
    format!("{user}@{host}")
}

/// A core dump of a process holding a value would be a value on disk.
pub fn no_core_dumps() {
    let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `none` outlives the call.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) };
}

/// The tail of a public key: what the owner compares between the caller's
/// terminal and the prompt, if they want to know the server did not swap it.
pub fn fingerprint(pubkey: &str) -> &str {
    &pubkey[pubkey.len().saturating_sub(16)..]
}

/// What a caller asks for.
pub struct Ask<'a> {
    pub url: &'a str,
    pub credential: &'a str,
    pub secret: &'a str,
    pub purpose: &'a str,
    pub caller: &'a str,
    pub poll: Duration,
    pub max_wait: Duration,
}

/// Makes a one-time key in memory, requests the secret, waits for the owner,
/// and opens the sealed result. Only the owner's approval ends in `Ok`.
pub async fn fetch(ask: Ask<'_>) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let identity = age::x25519::Identity::generate();
    let public = identity.to_public().to_string();

    let mut rpc = Rpc::connect(ask.url, ask.credential).await.context("connecting to the vault")?;
    let id = match rpc
        .call("request", json!({"secret": ask.secret, "purpose": ask.purpose, "one_time_pubkey": public, "caller": ask.caller}))
        .await?
    {
        VaultEvent::Requested { id, .. } => id,
        other => bail!("unexpected answer: {other:?}"),
    };
    eprintln!("waiting for the owner: request {id}, one-time key …{}", fingerprint(&public));

    let start = Instant::now();
    loop {
        tokio::time::sleep(ask.poll).await;
        if start.elapsed() > ask.max_wait {
            bail!("gave up after {}s", ask.max_wait.as_secs());
        }
        let answer = match rpc.call("result", json!({"id": id})).await {
            Ok(answer) => answer,
            // A refusal from a live server is final; a dead socket is not: the
            // request survives a restart, so reconnect and go on asking.
            Err(e) if !is_connection_failure(&e) => return Err(e),
            Err(_) => {
                if let Ok(fresh) = Rpc::connect(ask.url, ask.credential).await {
                    rpc = fresh;
                }
                continue;
            }
        };
        match answer {
            VaultEvent::Result { status: ResultStatus::Pending, .. } => {}
            VaultEvent::Result { status: ResultStatus::Denied, .. } => bail!("the owner denied the request"),
            VaultEvent::Result { status: ResultStatus::Expired, .. } => bail!("the request expired"),
            VaultEvent::Result { status: ResultStatus::Sealed, sealed: Some(sealed), .. } => return open(&identity, &sealed),
            other => bail!("unexpected answer: {other:?}"),
        }
    }
}

fn is_connection_failure(e: &anyhow::Error) -> bool {
    // `Rpc::call`'s own refusals have no "vault.<method>:" prefix.
    e.to_string().starts_with("vault.")
}

/// Opens a sealed result in memory.
pub fn open(identity: &age::x25519::Identity, sealed: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let reader = age::armor::ArmoredReader::new(sealed.as_bytes());
    let decryptor = age::Decryptor::new_buffered(reader).context("the sealed result is not age format")?;
    let mut plain = decryptor.decrypt(std::iter::once(identity as &dyn age::Identity)).context("the sealed result does not open with this key")?;
    // Sized up front: a Vec that grew would leave unzeroized copies behind.
    let mut value = Zeroizing::new(Vec::with_capacity(sealed.len()));
    plain.read_to_end(&mut value).context("reading the sealed result")?;
    Ok(value)
}
