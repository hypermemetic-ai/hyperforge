//! The WorkOS gate over the real binary and a real socket, against a fake
//! WorkOS on loopback. The server is told who it serves through its
//! environment. Every method is tried, because a method left out of the gate
//! is the failure this file exists to catch.

#[path = "support/idp.rs"]
mod idp;

use futures::StreamExt;
use idp::{Idp, CLIENT, OTHER_CLIENT, OTHER_USER, OWNER};
use plexus_client::{decode, ClientEvent, MethodRef, Transport, WsTransport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const VAULT: &str = env!("CARGO_BIN_EXE_hyperforge-vault");
const MACHINE_TOKEN: &str = "machine-token-publish-0123456789abcdef";
const SECOND_MACHINE_TOKEN: &str = "machine-token-mirror-fedcba9876543210";

fn sha_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

struct Vault {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    port: u16,
    child: Child,
}

impl Drop for Vault {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn command(root: &Path, port: u16, idp: Option<&Idp>, owner: Option<&str>) -> Command {
    let mut c = Command::new(VAULT);
    c.env_clear().env("PATH", "/usr/bin:/bin").env("PLEXUS_NO_REGISTRY", "1").env("RUST_LOG", "trace");
    c.env("VAULT_STATE_DIR", root.join("state")).env("VAULT_PORT", port.to_string());
    let machines = root.join("machines");
    fs::write(&machines, format!("# name, then the sha256 of its token\npublish {}\nmirror {}\n", sha_hex(MACHINE_TOKEN), sha_hex(SECOND_MACHINE_TOKEN))).unwrap();
    c.env("VAULT_MACHINE_CREDENTIALS", &machines);
    if let Some(idp) = idp {
        c.env("VAULT_WORKOS_CLIENT_ID", CLIENT).env("VAULT_WORKOS_BASE", &idp.base);
    }
    if let Some(owner) = owner {
        c.env("VAULT_OWNER_USER_ID", owner);
    }
    c.stdin(Stdio::null());
    c
}

fn start(idp: Option<&Idp>, owner: Option<&str>) -> Vault {
    let tmp = tempfile::Builder::new().prefix("vault-gate").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = tmp.path().to_path_buf();
    let port = free_port();
    let log = fs::File::create(root.join("server.log")).unwrap();
    let child = command(&root, port, idp, owner).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
    let mut vault = Vault { _tmp: tmp, root, port, child };
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", vault.port)).is_err() {
        if let Some(status) = vault.child.try_wait().unwrap() {
            panic!("the server exited: {status}");
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the server did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    vault
}

impl Vault {
    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
    fn audit(&self) -> String {
        fs::read_to_string(self.root.join("state/audit.jsonl")).unwrap_or_default()
    }
    fn log(&self) -> Vec<u8> {
        fs::read(self.root.join("server.log")).unwrap_or_default()
    }
}

/// The first event of a call: `Err` if the socket was refused, `Ok(None)` if
/// the method answered nothing (a list with nothing in it). A `watch` that
/// was let in has nothing to end it, so a quiet stream is an admission too.
fn first(url: &str, token: Option<&str>, method: &'static str, params: Value) -> Result<Option<Value>, String> {
    let headers: Vec<(String, String)> = token.map(|t| ("Authorization".to_string(), format!("Bearer {t}"))).into_iter().collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let t = WsTransport::connect_with_headers(url, "hyperforge-vault", &headers).await.map_err(|e| e.to_string())?;
        let mut events = decode::<Value>(t.call(MethodRef::new("vault", method), params));
        loop {
            match tokio::time::timeout(Duration::from_secs(2), events.next()).await {
                Err(_) | Ok(None) => return Ok(None),
                Ok(Some(Ok(ClientEvent::Data(value)))) => return Ok(Some(value)),
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => return Err(e.to_string()),
            }
        }
    })
}

fn refused(answer: &Result<Option<Value>, String>) -> bool {
    match answer {
        Err(_) => true,
        Ok(Some(v)) => v["type"] == "error" && !v["message"].as_str().unwrap_or("").starts_with("no such"),
        Ok(None) => false,
    }
}

fn ciphertext() -> String {
    age::encrypt_and_armor(&age::x25519::Identity::generate().to_public(), b"a throwaway").unwrap()
}

fn one_time_key() -> String {
    age::x25519::Identity::generate().to_public().to_string()
}

/// A secret and a pending request, stored by the owner.
fn seed(v: &Vault, owner: &str) -> String {
    let stored = first(&v.url(), Some(owner), "put", json!({"name": "seeded", "ciphertext": ciphertext()})).unwrap().unwrap();
    assert_eq!(stored["type"], "stored", "{stored}");
    let asked = first(&v.url(), Some(owner), "request", json!({"secret": "seeded", "purpose": "seed", "one_time_pubkey": one_time_key()})).unwrap().unwrap();
    assert_eq!(asked["type"], "requested", "{asked}");
    asked["id"].as_str().unwrap().to_string()
}

/// Every method there is.
fn every_method(id: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("put", json!({"name": "smuggled", "ciphertext": ciphertext()})),
        ("list", json!({})),
        ("request", json!({"secret": "seeded", "purpose": "smuggled", "one_time_pubkey": one_time_key()})),
        ("pending", json!({})),
        ("get_request", json!({"id": id})),
        ("fulfill", json!({"id": id, "sealed": ciphertext()})),
        ("deny", json!({"id": id})),
        ("result", json!({"id": id})),
        ("watch", json!({})),
        ("audit", json!({})),
    ]
}

fn assert_all_refused(v: &Vault, id: &str, token: Option<&str>, who: &str) {
    let audit = v.audit();
    for (method, params) in every_method(id) {
        let answer = first(&v.url(), token, method, params);
        assert!(refused(&answer), "{who} reached vault.{method}: {answer:?}");
    }
    assert_eq!(v.audit(), audit, "{who} changed something");
}

#[test]
fn no_token_is_refused_on_every_method() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    assert_all_refused(&v, &id, None, "no token");
}

#[test]
fn a_wrong_client_is_refused() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    let mut claims = idp.claims(OWNER);
    claims["client_id"] = json!(OTHER_CLIENT);
    assert_all_refused(&v, &id, Some(&idp.sign(&claims)), "another client's token, this issuer");
    let mut claims = idp.claims(OWNER);
    claims.as_object_mut().unwrap().remove("client_id");
    assert_all_refused(&v, &id, Some(&idp.sign(&claims)), "a token naming no client");
}

#[test]
fn a_wrong_issuer_is_refused() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    let mut claims = idp.claims(OWNER);
    claims["iss"] = json!(format!("{}/user_management/{OTHER_CLIENT}", idp.base));
    assert_all_refused(&v, &id, Some(&idp.sign(&claims)), "another issuer's token");
}

#[test]
fn an_expired_token_is_refused() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    assert_all_refused(&v, &id, Some(&idp.token_lasting(OWNER, -1)), "an expired token");
}

#[test]
fn a_forged_signature_is_refused() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    assert_all_refused(&v, &id, Some(&idp.forge(&idp.claims(OWNER))), "a forged token");
}

/// Menger's client is shared: this token is good, and the wrong person's.
#[test]
fn a_valid_token_for_the_wrong_user_is_refused() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    assert_all_refused(&v, &id, Some(&idp.token(OTHER_USER)), "another Menger user");
}

#[test]
fn the_right_user_is_admitted_on_every_method() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let owner = idp.token(OWNER);
    let id = seed(&v, &owner);
    let mut answered = Vec::new();
    for (method, params) in every_method(&id) {
        let answer = first(&v.url(), Some(&owner), method, params).unwrap().unwrap_or_else(|| panic!("{method} answered nothing"));
        // `fulfill` closes the request, so `deny` then says so: an answer
        // from past the gate, which is the point here.
        assert!(answer["type"] != "error" || answer["message"].as_str().unwrap().contains("not pending"), "{method}: {answer}");
        answered.push(method);
    }
    assert_eq!(answered.len(), 10);
}

#[test]
fn a_machine_may_request_and_read_its_own_result() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    seed(&v, &idp.token(OWNER));
    let asked = first(&v.url(), Some(MACHINE_TOKEN), "request", json!({"secret": "seeded", "purpose": "publish", "one_time_pubkey": one_time_key()})).unwrap().unwrap();
    assert_eq!(asked["type"], "requested", "{asked}");
    let id = asked["id"].as_str().unwrap();
    let result = first(&v.url(), Some(MACHINE_TOKEN), "result", json!({"id": id})).unwrap().unwrap();
    assert_eq!(result["status"], "pending", "{result}");
}

#[test]
fn a_machine_is_refused_everything_else() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    let asked = first(&v.url(), Some(MACHINE_TOKEN), "request", json!({"secret": "seeded", "purpose": "publish", "one_time_pubkey": one_time_key()})).unwrap().unwrap();
    let own = asked["id"].as_str().unwrap().to_string();
    let audit = v.audit();
    for (method, params) in every_method(&own) {
        if matches!(method, "request" | "result") {
            continue;
        }
        let answer = first(&v.url(), Some(MACHINE_TOKEN), method, params);
        assert!(refused(&answer), "a machine reached vault.{method}: {answer:?}");
    }
    assert_eq!(v.audit(), audit, "a machine changed something");
    assert_eq!(first(&v.url(), Some(&idp.token(OWNER)), "result", json!({"id": id})).unwrap().unwrap()["status"], "pending");
}

#[test]
fn a_machine_cannot_read_a_request_it_did_not_make() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let owners = seed(&v, &idp.token(OWNER));
    let asked = first(&v.url(), Some(SECOND_MACHINE_TOKEN), "request", json!({"secret": "seeded", "purpose": "mirror", "one_time_pubkey": one_time_key()})).unwrap().unwrap();
    let theirs = asked["id"].as_str().unwrap();
    for id in [owners.as_str(), theirs] {
        let answer = first(&v.url(), Some(MACHINE_TOKEN), "result", json!({"id": id})).unwrap().unwrap();
        assert_eq!(answer["type"], "error", "{answer}");
        // The same words as an id that never existed: no oracle.
        assert_eq!(answer, first(&v.url(), Some(MACHINE_TOKEN), "result", json!({"id": "no-such-id"})).unwrap().unwrap());
    }
}

#[test]
fn only_the_token_opens_the_door_not_its_hash() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let id = seed(&v, &idp.token(OWNER));
    assert_all_refused(&v, &id, Some(&sha_hex(MACHINE_TOKEN)), "the stored hash");
    assert_all_refused(&v, &id, Some("machine-token-publish"), "a wrong token");
}

#[test]
fn no_configuration_means_nobody() {
    let idp = Idp::start();
    for (client, owner) in [(None, None), (Some(&idp), None), (None, Some(OWNER))] {
        let v = start(client, owner);
        for token in [None, Some(MACHINE_TOKEN.to_string()), Some(idp.token(OWNER))] {
            for (method, params) in every_method("nothing") {
                let answer = first(&v.url(), token.as_deref(), method, params);
                assert!(refused(&answer), "vault.{method} with {token:?}: {answer:?}");
            }
        }
    }
}

/// A plain JSON-RPC POST is the same port by another door.
#[test]
fn a_plain_http_post_is_refused_too() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    seed(&v, &idp.token(OWNER));
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"hyperforge-vault.call","params":{"method":"vault.list","params":{}}}"#;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", v.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    assert!(text.starts_with("HTTP/1.1 401"), "{text}");
}

/// The token ends and so does the socket, with a watch open on it.
#[test]
fn a_socket_closes_when_its_token_ends() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let token = idp.token_lasting(OWNER, 3);
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let t = WsTransport::connect_with_headers(&v.url(), "hyperforge-vault", &headers).await.unwrap();
        let mut events = decode::<Value>(t.call(MethodRef::new("vault", "watch"), json!({})));
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.next().await {
                if event.is_err() {
                    break;
                }
            }
        });
        assert!(ended.await.is_ok(), "the watch outlived its token");
    });
}

#[test]
fn an_unsafe_bind_needs_saying_so() {
    let idp = Idp::start();
    let tmp = tempfile::Builder::new().prefix("vault-gate").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let port = free_port();
    let mut refused = command(tmp.path(), port, Some(&idp), Some(OWNER)).args(["--bind", "0.0.0.0"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let start = Instant::now();
    while refused.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(3) {
            let _ = refused.kill();
            panic!("the server is listening on every interface");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = refused.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--allow-unsafe-bind"), "{}", String::from_utf8_lossy(&out.stderr));

    let mut allowed = command(tmp.path(), port, Some(&idp), Some(OWNER)).args(["--bind", "0.0.0.0", "--allow-unsafe-bind"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(allowed.try_wait().unwrap().is_none(), "the flag did not let it start");
        assert!(start.elapsed() < Duration::from_secs(20), "the server did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = allowed.kill();
    let _ = allowed.wait();
}

/// RUST_LOG=trace, every credential presented, good and bad: none in the log.
#[test]
fn bearer_tokens_never_reach_the_server_log() {
    let idp = Idp::start();
    let v = start(Some(&idp), Some(OWNER));
    let owner = idp.token(OWNER);
    let id = seed(&v, &owner);
    let wrong = idp.token(OTHER_USER);
    let forged = idp.forge(&idp.claims(OWNER));
    for token in [&owner, &wrong, &forged] {
        let _ = first(&v.url(), Some(token), "get_request", json!({"id": id}));
    }
    let _ = first(&v.url(), Some(MACHINE_TOKEN), "result", json!({"id": id}));
    let log = v.log();
    assert!(!log.is_empty());
    let hay = String::from_utf8_lossy(&log);
    for token in [&owner, &wrong, &forged] {
        // The signature is the part no one else can make, and the session id
        // of a token with neither `sid` nor `jti` falls back to it.
        let signature = token.rsplit('.').next().unwrap();
        assert!(!hay.contains(signature), "a token's signature is in the log");
        let payload = token.split('.').nth(1).unwrap();
        assert!(!hay.contains(payload), "a token's claims are in the log");
    }
    for token in [MACHINE_TOKEN, SECOND_MACHINE_TOKEN] {
        assert!(!hay.contains(token), "a machine credential is in the log");
    }
}
