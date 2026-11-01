//! The seam over the real transport, with a test double for the identity
//! provider: the case HYPE-17 brings and a single local credential cannot
//! show, a caller who is authenticated and is not the owner.

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
    let key = age::x25519::Identity::generate().to_public();
    let ciphertext = age::encrypt_and_armor(&key, b"a throwaway").unwrap();
    store.put("s", &ciphertext).unwrap();
    let (id, _) = store.request("s", "seam", &key.to_string(), None).unwrap();

    let service = Arc::new(DynamicHub::new(SERVICE).register(VaultActivation::new(store).with_owner(gate)));
    let module = DynamicHub::arc_into_rpc_module(service).unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = WebSocketConfig::new(0);
    config.addr = addr;
    let options = UpgradeOptions { anonymous_view: true, ..UpgradeOptions::default() };
    let (handle, bound) = serve_websocket_with(module, config, Some(Arc::new(Double)), options).await.unwrap();
    Vault { url: format!("ws://{bound}"), id, _dir: dir, _handle: handle }
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

#[tokio::test]
async fn an_authenticated_non_owner_is_refused() {
    let v = vault(gate()).await;
    for (method, params) in all_three(&v.id) {
        let answer = call(&v.url, Some(OTHER), method, params).await;
        assert_eq!(answer["type"], "error", "{method}: {answer}");
    }
    assert_eq!(call(&v.url, None, "result", json!({"id": v.id})).await["status"], "pending");
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
