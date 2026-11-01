//! The seam over the real transport, with a test double for the identity
//! provider: the case HYPE-17 brings and a single local credential cannot
//! show, a caller who is authenticated and is not the owner. The double lets
//! every socket in, so what is tested here is the check inside each method,
//! with the upgrade's own refusal out of the way.

use async_trait::async_trait;
use futures::StreamExt;
use hyperforge_vault::owner::OwnerGate;
use hyperforge_vault::server::{activation::VaultActivation, Store};
use hyperforge_vault::wire::SERVICE;
use plexus_client::{decode, ClientEvent, MethodRef, Transport, WsTransport};
use plexus_core::plexus::{AuthContext, DynamicHub, SessionValidator};
use plexus_transport::websocket::serve_websocket_with;
use plexus_transport::{UpgradeOptions, WebSocketConfig};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const OWNER: &str = "oidc:workos/user_owner";
const OTHER: &str = "oidc:workos/user_other";
const MACHINE: &str = "apikey:vault-machine-publish";

/// Stands in for `OidcValidator`: the token is the principal.
struct Double;

#[async_trait]
impl SessionValidator for Double {
    async fn validate(&self, token: &str) -> Option<AuthContext> {
        Some(AuthContext::for_principal(&token.parse().ok()?, "double", Vec::new(), Value::Null))
    }
}

struct Vault {
    url: String,
    id: String,
    _dir: tempfile::TempDir,
    _handle: jsonrpsee::server::ServerHandle,
}

async fn vault(gate: OwnerGate) -> Vault {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), Duration::from_secs(300), Duration::from_secs(60)).unwrap());
    let service = Arc::new(DynamicHub::new(SERVICE).register(VaultActivation::new(store).with_owner(gate.clone())));
    let module = DynamicHub::arc_into_rpc_module(service).unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = WebSocketConfig::new(0);
    config.addr = addr;
    let options = UpgradeOptions { anonymous_view: true, ..UpgradeOptions::default() };
    let (handle, bound) = serve_websocket_with(module, config, Some(Arc::new(Double)), options).await.unwrap();
    let url = format!("ws://{bound}");
    // Seeded over the wire as the owner: nothing else may store or ask.
    let seeder = if gate.is_configured() { OWNER } else { "" };
    let id = seed(&url, seeder).await;
    Vault { url, id, _dir: dir, _handle: handle }
}

async fn seed(url: &str, owner: &str) -> String {
    let key = age::x25519::Identity::generate().to_public();
    let ciphertext = age::encrypt_and_armor(&key, b"a throwaway").unwrap();
    if owner.is_empty() {
        return "no-request".to_string();
    }
    assert_eq!(call(url, Some(owner), "put", json!({"name": "s", "ciphertext": ciphertext})).await["type"], "stored");
    let asked = call(url, Some(owner), "request", json!({"secret": "s", "purpose": "seam", "one_time_pubkey": key.to_string()})).await;
    asked["id"].as_str().unwrap().to_string()
}

async fn call(url: &str, token: Option<&str>, method: &'static str, params: Value) -> Value {
    let headers: Vec<(String, String)> = token.map(|t| ("Authorization".to_string(), format!("Bearer {t}"))).into_iter().collect();
    let transport = WsTransport::connect_with_headers(url, SERVICE, &headers).await.unwrap();
    let mut events = decode::<Value>(transport.call(MethodRef::new("vault", method), params));
    while let Some(event) = events.next().await {
        if let ClientEvent::Data(value) = event.unwrap() {
            return value;
        }
    }
    panic!("{method} answered nothing");
}

fn all_three(id: &str) -> [(&'static str, Value); 3] {
    let key = age::x25519::Identity::generate().to_public();
    let sealed = age::encrypt_and_armor(&key, b"forged").unwrap();
    [("get_request", json!({"id": id})), ("fulfill", json!({"id": id, "sealed": sealed})), ("deny", json!({"id": id}))]
}

fn gate() -> OwnerGate {
    OwnerGate::owned_by(OWNER.parse().unwrap())
}

/// Everything but the three: the seven HYPE-16 left open.
fn the_other_seven(id: &str) -> [(&'static str, Value); 7] {
    let key = age::x25519::Identity::generate().to_public();
    [
        ("put", json!({"name": "smuggled", "ciphertext": age::encrypt_and_armor(&key, b"smuggled").unwrap()})),
        ("list", json!({})),
        ("request", json!({"secret": "s", "purpose": "smuggled", "one_time_pubkey": key.to_string()})),
        ("pending", json!({})),
        ("watch", json!({})),
        ("result", json!({"id": id})),
        ("audit", json!({})),
    ]
}

#[tokio::test]
async fn every_method_checks_for_itself() {
    let v = vault(gate()).await;
    for caller in [None, Some(OTHER)] {
        for (method, params) in the_other_seven(&v.id) {
            let answer = call(&v.url, caller, method, params).await;
            assert_eq!(answer["type"], "error", "{method} as {caller:?}: {answer}");
        }
    }
}

/// A machine holds a valid session and is on the allow-list. Neither is being
/// the owner: it may ask and collect, and nothing else.
#[tokio::test]
async fn a_machine_is_not_the_owner() {
    let v = vault(gate()).await;
    let methods = all_three(&v.id).into_iter().chain(the_other_seven(&v.id));
    for (method, params) in methods {
        if matches!(method, "request" | "result") {
            continue;
        }
        let answer = call(&v.url, Some(MACHINE), method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
    assert_eq!(call(&v.url, Some(OWNER), "result", json!({"id": v.id})).await["status"], "pending");
}

#[tokio::test]
async fn an_unconfigured_gate_refuses_a_machine_too() {
    let v = vault(OwnerGate::default()).await;
    for (method, params) in the_other_seven(&v.id) {
        let answer = call(&v.url, Some(MACHINE), method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
}

#[tokio::test]
async fn an_authenticated_non_owner_is_refused() {
    let v = vault(gate()).await;
    for (method, params) in all_three(&v.id) {
        let answer = call(&v.url, Some(OTHER), method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
    assert_eq!(call(&v.url, Some(OWNER), "result", json!({"id": v.id})).await["status"], "pending");
}

#[tokio::test]
async fn an_anonymous_socket_is_refused() {
    let v = vault(gate()).await;
    for (method, params) in all_three(&v.id) {
        let answer = call(&v.url, None, method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
}

/// The positive control: the identity does reach the method, over a real
/// socket. This is the evidence the seam stands on.
#[tokio::test]
async fn the_owner_is_admitted() {
    let v = vault(gate()).await;
    assert_eq!(call(&v.url, Some(OWNER), "get_request", json!({"id": v.id})).await["type"], "request");
    assert_eq!(call(&v.url, Some(OWNER), "deny", json!({"id": v.id})).await["type"], "denied");
}

#[tokio::test]
async fn the_default_activation_refuses_even_a_valid_session() {
    let v = vault(OwnerGate::default()).await;
    for (method, params) in all_three(&v.id) {
        let answer = call(&v.url, Some(OWNER), method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
}
