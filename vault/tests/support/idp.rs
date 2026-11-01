//! A fake WorkOS on loopback: discovery, a JWKS, and the authenticate
//! endpoint. Nothing here reaches WorkOS. The keys in `keys.rs` are
//! plexus-auth-core's published test fixtures, private to no one.

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "keys.rs"]
mod keys;

pub const CLIENT: &str = "client_test_vault";
pub const OTHER_CLIENT: &str = "client_test_other";
pub const OWNER: &str = "user_test_owner";
pub const OTHER_USER: &str = "user_test_other";

#[derive(Default)]
struct State {
    /// The refresh token that is still good, and what it mints next.
    refresh: Option<String>,
    /// How long a refreshed access token lasts.
    refreshed_ttl: i64,
    refreshes: u32,
    /// A login in flight: the code, and the challenge it was issued against.
    code: Option<(String, String)>,
}

pub struct Idp {
    pub base: String,
    state: Arc<Mutex<State>>,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Random enough to tell two tokens apart, and to be a needle for the leak scan.
pub fn nonce() -> String {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut bytes).unwrap();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Idp {
    pub fn start() -> Idp {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(State { refreshed_ttl: 3600, ..State::default() }));
        let (thread_base, thread_state) = (base.clone(), state.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (base, state) = (thread_base.clone(), thread_state.clone());
                std::thread::spawn(move || serve(stream, &base, &state));
            }
        });
        Idp { base, state }
    }

    pub fn issuer(&self) -> String {
        format!("{}/user_management/{CLIENT}", self.base)
    }

    pub fn claims(&self, user: &str) -> Value {
        let now = now();
        json!({"iss": self.issuer(), "sub": user, "client_id": CLIENT, "iat": now - 5, "exp": now + 3600, "jti": nonce()})
    }

    pub fn sign(&self, claims: &Value) -> String {
        sign(keys::KID_1, keys::KEY_1_PEM, claims)
    }

    /// Signed by a key the JWKS does not hold, under a kid it does.
    pub fn forge(&self, claims: &Value) -> String {
        sign(keys::KID_1, keys::KEY_2_PEM, claims)
    }

    pub fn token(&self, user: &str) -> String {
        self.sign(&self.claims(user))
    }

    pub fn token_lasting(&self, user: &str, seconds: i64) -> String {
        let mut claims = self.claims(user);
        claims["exp"] = json!(now() + seconds);
        self.sign(&claims)
    }

    /// Declares `refresh` good, once, and how long what it mints lasts.
    pub fn accept_refresh(&self, refresh: &str, ttl: i64) {
        let mut state = self.state.lock().unwrap();
        state.refresh = Some(refresh.to_string());
        state.refreshed_ttl = ttl;
    }

    pub fn refreshes(&self) -> u32 {
        self.state.lock().unwrap().refreshes
    }

    /// The browser's half of a login: WorkOS would issue this code.
    pub fn issue_code(&self, challenge: &str) -> String {
        let code = nonce();
        self.state.lock().unwrap().code = Some((code.clone(), challenge.to_string()));
        code
    }
}

fn sign(kid: &str, pem: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()).unwrap()
}

fn answer(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn serve(mut stream: TcpStream, base: &str, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let path = first.split_whitespace().nth(1).unwrap_or("").to_string();

    if path == format!("/user_management/{CLIENT}/.well-known/openid-configuration") {
        let doc = json!({"issuer": format!("{base}/user_management/{CLIENT}"), "jwks_uri": format!("{base}/sso/jwks/{CLIENT}")});
        answer(&mut stream, "200 OK", &doc.to_string());
    } else if path == format!("/sso/jwks/{CLIENT}") {
        answer(&mut stream, "200 OK", &format!(r#"{{"keys": [{}]}}"#, keys::JWK_1));
    } else if path == "/user_management/authenticate" {
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        match authenticate(&request, base, state) {
            Some(tokens) => answer(&mut stream, "200 OK", &tokens.to_string()),
            None => answer(&mut stream, "400 Bad Request", r#"{"error":"invalid_grant"}"#),
        }
    } else {
        answer(&mut stream, "404 Not Found", "{}");
    }
}

/// The two grants vault-login and the refresh path use, checked the way WorkOS
/// checks them: a public client, no secret, a verifier that matches, and a
/// refresh token that works once.
fn authenticate(request: &Value, base: &str, state: &Mutex<State>) -> Option<Value> {
    if request["client_id"] != CLIENT || request.get("client_secret").is_some() {
        return None;
    }
    let mut state = state.lock().unwrap();
    match request["grant_type"].as_str()? {
        "authorization_code" => {
            let (code, challenge) = state.code.take()?;
            let verifier = request["code_verifier"].as_str()?;
            if request["code"] != code.as_str() || b64url(&Sha256::digest(verifier.as_bytes())) != challenge {
                return None;
            }
        }
        "refresh_token" => {
            if state.refresh.as_deref() != request["refresh_token"].as_str() || state.refresh.is_none() {
                return None;
            }
            state.refreshes += 1;
        }
        _ => return None,
    }
    let next = format!("refresh-{}", nonce());
    state.refresh = Some(next.clone());
    let now = now();
    let claims = json!({"iss": format!("{base}/user_management/{CLIENT}"), "sub": OWNER, "client_id": CLIENT, "iat": now - 5, "exp": now + state.refreshed_ttl, "jti": nonce()});
    Some(json!({"access_token": sign(keys::KID_1, keys::KEY_1_PEM, &claims), "refresh_token": next, "user": {"id": OWNER}}))
}
