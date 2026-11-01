//! The vault. It stores ciphertext and moves sealed results; it has no key, and
//! nothing here can decrypt (the end-to-end suite holds this file to that).
//!
//! State, under `--state-dir`:
//! - `secrets/<name>.age`: the ciphertexts.
//! - `requests/<id>.json`: one request each, replaced atomically on every
//!   change, so a restart loses nothing and a sealed result leaves disk with
//!   the rename that retires it.
//! - `audit.jsonl`: append-only.

use crate::wire::*;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// How long a closed request's record is kept, so `result` can say why.
const TOMBSTONE_MS: u64 = 24 * 60 * 60 * 1000;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Fulfilled,
    Denied,
    Expired,
    Delivered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    summary: RequestSummary,
    status: Status,
    /// The sealed result, only while `status` is `Fulfilled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed: Option<String>,
    /// When an unread result goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result_expires_at: Option<u64>,
    /// When the request reached a closed status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_at: Option<u64>,
}

struct Inner {
    requests: HashMap<String, Record>,
    audit: File,
}

pub struct Store {
    dir: PathBuf,
    request_ttl: Duration,
    result_ttl: Duration,
    inner: Mutex<Inner>,
    pending_tx: broadcast::Sender<RequestSummary>,
}

fn mkdir(path: &Path) -> anyhow::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path).with_context(|| format!("creating {}", path.display()))
}

/// `bytes` at `path`, whole or not at all, and on disk when this returns.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().context("no parent")?)?.sync_all()?;
    Ok(())
}

impl Store {
    pub fn open(dir: &Path, request_ttl: Duration, result_ttl: Duration) -> anyhow::Result<Store> {
        mkdir(dir)?;
        mkdir(&dir.join("secrets"))?;
        mkdir(&dir.join("requests"))?;
        let audit = OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("audit.jsonl")).context("opening the audit log")?;
        let mut requests = HashMap::new();
        for entry in fs::read_dir(dir.join("requests"))? {
            let path = entry?.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("json") => {
                    let text = fs::read_to_string(&path)?;
                    let record: Record = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
                    requests.insert(record.summary.id.clone(), record);
                }
                // A write the last run did not finish; its request is intact.
                Some("tmp") => fs::remove_file(&path)?,
                _ => {}
            }
        }
        let (pending_tx, _) = broadcast::channel(MAX_PENDING * 2);
        let store = Store { dir: dir.to_path_buf(), request_ttl, result_ttl, inner: Mutex::new(Inner { requests, audit }), pending_tx };
        store.sweep()?;
        Ok(store)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join("requests").join(format!("{id}.json"))
    }

    fn secret_path(&self, name: &str) -> PathBuf {
        self.dir.join("secrets").join(format!("{name}.age"))
    }

    fn persist(&self, record: &Record) -> anyhow::Result<()> {
        write_atomic(&self.record_path(&record.summary.id), &serde_json::to_vec(record)?)
    }

    fn audit(&self, inner: &mut Inner, event: &str, secret: &str, summary: Option<&RequestSummary>) -> anyhow::Result<()> {
        let entry = AuditEntry {
            ts: now_ms(),
            event: event.into(),
            request_id: summary.map(|s| s.id.clone()),
            secret: secret.into(),
            purpose: summary.map(|s| s.purpose.clone()),
            caller: summary.map(|s| s.caller.clone()),
        };
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        inner.audit.write_all(&line).context("writing the audit log")?;
        inner.audit.sync_data().context("syncing the audit log")?;
        Ok(())
    }

    /// Closes what has lapsed and forgets old tombstones. Every operation
    /// starts here, so a lapsed request is lapsed on the instant, not at the
    /// next tick.
    pub fn sweep(&self) -> anyhow::Result<()> {
        let now = now_ms();
        let mut inner = self.lock();
        let inner = &mut *inner;
        let due: Vec<String> = inner
            .requests
            .values()
            .filter(|r| match r.status {
                Status::Pending => now >= r.summary.expires_at,
                Status::Fulfilled => r.result_expires_at.is_some_and(|t| now >= t),
                _ => false,
            })
            .map(|r| r.summary.id.clone())
            .collect();
        for id in due {
            let record = inner.requests.get_mut(&id).context("gone")?;
            record.status = Status::Expired;
            record.sealed = None;
            record.closed_at = Some(now);
            let record = record.clone();
            self.persist(&record)?;
            self.audit(inner, "expire", &record.summary.secret, Some(&record.summary))?;
        }
        let old: Vec<String> = inner.requests.values().filter(|r| r.closed_at.is_some_and(|t| now >= t + TOMBSTONE_MS)).map(|r| r.summary.id.clone()).collect();
        for id in old {
            fs::remove_file(self.record_path(&id))?;
            inner.requests.remove(&id);
        }
        Ok(())
    }

    pub fn put(&self, name: &str, ciphertext: &str) -> anyhow::Result<()> {
        if !valid_secret_name(name) {
            bail!("'{}' is not a secret name: 1 to 64 of A-Z a-z 0-9 . _ -, starting with a letter or digit", printable(name, 70));
        }
        if ciphertext.len() > MAX_CIPHERTEXT {
            bail!("the ciphertext is larger than {MAX_CIPHERTEXT} bytes");
        }
        if !is_age_format(ciphertext) {
            bail!("not age format: the server stores ciphertext only, encrypt on the client");
        }
        let mut inner = self.lock();
        let path = self.secret_path(name);
        // An overwrite is how a tailnet device would swap a secret for a value
        // it chose. Rotation is a deliberate act on the server (README).
        if path.exists() {
            bail!("'{name}' already exists; a stored secret is never replaced over the wire");
        }
        write_atomic(&path, ciphertext.as_bytes())?;
        self.audit(&mut inner, "put", name, None)
    }

    pub fn list(&self) -> anyhow::Result<Vec<(String, u64, u64)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join("secrets"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("age") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let meta = entry.metadata()?;
            let stored = meta.modified()?.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            out.push((name.to_string(), meta.len(), stored));
        }
        out.sort();
        Ok(out)
    }

    pub fn request(&self, secret: &str, purpose: &str, one_time_pubkey: &str, caller: Option<&str>) -> anyhow::Result<(String, u64)> {
        self.sweep()?;
        if !valid_secret_name(secret) || !self.secret_path(secret).exists() {
            bail!("no secret '{}'", printable(secret, 70));
        }
        if purpose.trim().is_empty() {
            bail!("a request needs a purpose");
        }
        if purpose.chars().count() > MAX_PURPOSE {
            bail!("the purpose is longer than {MAX_PURPOSE} characters");
        }
        let caller = caller.unwrap_or("unknown");
        if caller.chars().count() > MAX_CALLER {
            bail!("the caller is longer than {MAX_CALLER} characters");
        }
        if !valid_one_time_pubkey(one_time_pubkey) {
            bail!("one_time_pubkey is not an X25519 age recipient");
        }
        let mut inner = self.lock();
        let inner = &mut *inner;
        if inner.requests.values().filter(|r| r.status == Status::Pending).count() >= MAX_PENDING {
            bail!("too many requests are pending");
        }
        let now = now_ms();
        let summary = RequestSummary {
            id: uuid::Uuid::new_v4().to_string(),
            secret: secret.into(),
            purpose: purpose.into(),
            caller: caller.into(),
            one_time_pubkey: one_time_pubkey.into(),
            created_at: now,
            expires_at: now + self.request_ttl.as_millis() as u64,
        };
        let record = Record { summary: summary.clone(), status: Status::Pending, sealed: None, result_expires_at: None, closed_at: None };
        self.persist(&record)?;
        inner.requests.insert(summary.id.clone(), record);
        self.audit(inner, "request", secret, Some(&summary))?;
        let _ = self.pending_tx.send(summary.clone());
        Ok((summary.id, summary.expires_at))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RequestSummary> {
        self.pending_tx.subscribe()
    }

    pub fn pending(&self) -> anyhow::Result<Vec<RequestSummary>> {
        self.sweep()?;
        let inner = self.lock();
        let mut out: Vec<_> = inner.requests.values().filter(|r| r.status == Status::Pending).map(|r| r.summary.clone()).collect();
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    /// The pending record for `id`, or why not.
    fn pending_record<'a>(inner: &'a mut Inner, id: &str) -> anyhow::Result<&'a mut Record> {
        let record = inner.requests.get_mut(id).context("no such request")?;
        if record.status != Status::Pending {
            bail!("the request is not pending ({:?})", record.status);
        }
        Ok(record)
    }

    pub fn get_request(&self, id: &str) -> anyhow::Result<(RequestSummary, String)> {
        self.sweep()?;
        let mut inner = self.lock();
        let record = Self::pending_record(&mut inner, id)?;
        let ciphertext = fs::read_to_string(self.secret_path(&record.summary.secret)).context("reading the ciphertext")?;
        Ok((record.summary.clone(), ciphertext))
    }

    pub fn fulfill(&self, id: &str, sealed: &str) -> anyhow::Result<()> {
        self.sweep()?;
        if sealed.len() > MAX_CIPHERTEXT || !is_age_format(sealed) {
            bail!("the sealed result is not age format");
        }
        let mut inner = self.lock();
        let inner = &mut *inner;
        let now = now_ms();
        let record = Self::pending_record(inner, id)?;
        record.status = Status::Fulfilled;
        record.sealed = Some(sealed.to_string());
        record.result_expires_at = Some(now + self.result_ttl.as_millis() as u64);
        let record = record.clone();
        self.persist(&record)?;
        // The approval happened on the Mac; a fulfil is how the server learns of it.
        self.audit(inner, "approve", &record.summary.secret, Some(&record.summary))?;
        self.audit(inner, "fulfill", &record.summary.secret, Some(&record.summary))
    }

    pub fn deny(&self, id: &str) -> anyhow::Result<()> {
        self.sweep()?;
        let mut inner = self.lock();
        let inner = &mut *inner;
        let record = Self::pending_record(inner, id)?;
        record.status = Status::Denied;
        record.closed_at = Some(now_ms());
        let record = record.clone();
        self.persist(&record)?;
        self.audit(inner, "deny", &record.summary.secret, Some(&record.summary))
    }

    /// The status, and on the one read of a fulfilled request, the sealed
    /// result: it is off disk before it is returned.
    pub fn result(&self, id: &str) -> anyhow::Result<(ResultStatus, Option<String>)> {
        self.sweep()?;
        let mut inner = self.lock();
        let inner = &mut *inner;
        let record = inner.requests.get_mut(id).context("no such request")?;
        match record.status {
            Status::Pending => Ok((ResultStatus::Pending, None)),
            Status::Denied => Ok((ResultStatus::Denied, None)),
            Status::Expired => Ok((ResultStatus::Expired, None)),
            Status::Delivered => bail!("the result was already read; it is single-use"),
            Status::Fulfilled => {
                let sealed = record.sealed.take();
                record.status = Status::Delivered;
                record.closed_at = Some(now_ms());
                let record = record.clone();
                self.persist(&record)?;
                self.audit(inner, "deliver", &record.summary.secret, Some(&record.summary))?;
                Ok((ResultStatus::Sealed, sealed))
            }
        }
    }

    /// The last `limit` audit lines (all, without one).
    pub fn audit_log(&self, limit: Option<u32>) -> anyhow::Result<Vec<AuditEntry>> {
        let text = {
            let _inner = self.lock();
            fs::read_to_string(self.dir.join("audit.jsonl"))?
        };
        let mut entries = text.lines().map(serde_json::from_str).collect::<Result<Vec<AuditEntry>, _>>()?;
        if let Some(limit) = limit {
            entries.drain(..entries.len().saturating_sub(limit as usize));
        }
        Ok(entries)
    }
}

pub mod activation {
    //! The Plexus face of [`super::Store`].

    use super::Store;
    use crate::wire::*;
    use async_stream::stream;
    use futures::Stream;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::RecvError;

    fn refused(e: anyhow::Error) -> VaultEvent {
        VaultEvent::Error { message: format!("{e:#}") }
    }

    #[derive(Clone)]
    pub struct VaultActivation {
        store: Arc<Store>,
    }

    impl VaultActivation {
        pub fn new(store: Arc<Store>) -> Self {
            VaultActivation { store }
        }
    }

    #[plexus_macros::activation(
        namespace = "vault",
        description = "Ciphertext only: each read of a secret is approved by its owner",
        crate_path = "plexus_core"
    )]
    impl VaultActivation {
        #[plexus_macros::method(
            description = "Store a ciphertext under a new name; anything not age format is refused",
            params(name = "The secret's name", ciphertext = "An age file, armored")
        )]
        pub async fn put(&self, name: String, ciphertext: String) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.put(&name, &ciphertext) {
                Ok(()) => VaultEvent::Stored { name },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(streaming, description = "The stored secrets: names and sizes")]
        pub async fn list(&self) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let listed = self.store.list();
            stream! {
                match listed {
                    Ok(secrets) => for (name, size, stored_at) in secrets {
                        yield VaultEvent::Secret { name, size, stored_at };
                    },
                    Err(e) => yield refused(e),
                }
            }
        }

        #[plexus_macros::method(
            description = "Ask the owner for a secret, sealed to a one-time key",
            params(
                secret = "The secret's name",
                purpose = "Why: shown to the owner, unverified",
                one_time_pubkey = "The X25519 age recipient to seal to",
                caller = "Who is asking: shown to the owner, unverified"
            )
        )]
        pub async fn request(&self, secret: String, purpose: String, one_time_pubkey: String, caller: Option<String>) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.request(&secret, &purpose, &one_time_pubkey, caller.as_deref()) {
                Ok((id, expires_at)) => VaultEvent::Requested { id, expires_at },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(streaming, description = "The requests waiting for the owner")]
        pub async fn pending(&self) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let pending = self.store.pending();
            stream! {
                match pending {
                    Ok(requests) => for request in requests {
                        yield VaultEvent::Pending { request };
                    },
                    Err(e) => yield refused(e),
                }
            }
        }

        #[plexus_macros::method(description = "A pending request and its ciphertext, for the approver", params(id = "The request"))]
        pub async fn get_request(&self, id: String) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.get_request(&id) {
                Ok((request, ciphertext)) => VaultEvent::Request { request, ciphertext },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(description = "Answer a pending request with the value sealed to its one-time key", params(id = "The request", sealed = "An age file, armored"))]
        pub async fn fulfill(&self, id: String, sealed: String) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.fulfill(&id, &sealed) {
                Ok(()) => VaultEvent::Fulfilled { id },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(description = "Refuse a pending request", params(id = "The request"))]
        pub async fn deny(&self, id: String) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.deny(&id) {
                Ok(()) => VaultEvent::Denied { id },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(description = "A request's outcome; a sealed result is returned once and deleted", params(id = "The request"))]
        pub async fn result(&self, id: String) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let event = match self.store.result(&id) {
                Ok((status, sealed)) => VaultEvent::Result { id, status, sealed },
                Err(e) => refused(e),
            };
            stream! { yield event; }
        }

        #[plexus_macros::method(streaming, description = "The requests waiting now, then each new one, for the approver")]
        pub async fn watch(&self) -> impl Stream<Item = VaultEvent> + Send + 'static {
            // Subscribed before the snapshot: a request between the two is seen twice, not never.
            let mut new = self.store.subscribe();
            let store = self.store.clone();
            stream! {
                let mut seen = HashSet::new();
                match store.pending() {
                    Ok(requests) => for request in requests {
                        seen.insert(request.id.clone());
                        yield VaultEvent::Pending { request };
                    },
                    Err(e) => { yield refused(e); return; }
                }
                loop {
                    match new.recv().await {
                        Ok(request) => if seen.insert(request.id.clone()) {
                            yield VaultEvent::Pending { request };
                        },
                        Err(RecvError::Lagged(_)) => match store.pending() {
                            Ok(requests) => for request in requests {
                                if seen.insert(request.id.clone()) {
                                    yield VaultEvent::Pending { request };
                                }
                            },
                            Err(e) => { yield refused(e); return; }
                        },
                        Err(RecvError::Closed) => return,
                    }
                }
            }
        }

        #[plexus_macros::method(streaming, description = "The audit log, oldest first", params(limit = "Only the last this many lines"))]
        pub async fn audit(&self, limit: Option<u32>) -> impl Stream<Item = VaultEvent> + Send + 'static {
            let entries = self.store.audit_log(limit);
            stream! {
                match entries {
                    Ok(entries) => for entry in entries {
                        yield VaultEvent::Audit { entry };
                    },
                    Err(e) => yield refused(e),
                }
            }
        }
    }
}
