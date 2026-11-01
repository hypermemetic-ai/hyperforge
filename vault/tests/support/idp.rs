//! A fake WorkOS on loopback: discovery, a JWKS, and the authenticate
//! endpoint. Nothing here reaches WorkOS. The keys in `keys.rs` are
//! plexus-auth-core's published test fixtures, private to no one.

#![allow(dead_code)]

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
    /// What `POST /user_management/authorize/device` answers.
    authorize: Option<Reply>,
    /// What each poll of the device grant answers, in turn; the last repeats.
    polls: VecDeque<Reply>,
    /// Every device request, as it arrived.
    seen_authorize: Vec<Seen>,
    seen_polls: Vec<Seen>,
}

/// One answer from the fake.
#[derive(Clone)]
pub enum Reply {
    /// The OAuth error WorkOS polls are answered with, as a 400.
    Error(&'static str),
    /// A device grant that worked.
    Tokens { access: String, refresh: String },
    /// Anything at all.
    Raw(&'static str, String),
}

/// A request to the device endpoints, for what a test asserts on.
#[derive(Clone, Debug)]
pub struct Seen {
    pub at: Instant,
    pub content_type: String,
    pub raw: String,
    pub form: HashMap<String, String>,
}

pub struct Idp {
    pub base: String,
    state: Arc<Mutex<State>>,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
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

    /// Starts answering the device endpoint with `authorize`.
    pub fn device(&self, authorize: Value) {
        self.state.lock().unwrap().authorize = Some(Reply::Raw("200 OK", authorize.to_string()));
    }

    /// Answers the device endpoint with anything at all.
    pub fn device_raw(&self, status: &'static str, body: &str) {
        self.state.lock().unwrap().authorize = Some(Reply::Raw(status, body.to_string()));
    }

    /// What the polls answer, in turn; the last repeats.
    pub fn polls(&self, replies: Vec<Reply>) {
        self.state.lock().unwrap().polls = replies.into();
    }

    pub fn seen_authorize(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen_authorize.clone()
    }

    pub fn seen_polls(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen_polls.clone()
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

fn send(stream: &mut TcpStream, reply: Reply) {
    match reply {
        Reply::Error(code) => answer(stream, "400 Bad Request", &json!({"error": code, "error_description": "for a human"}).to_string()),
        Reply::Tokens { access, refresh } => {
            // The shape the docs show: `user` and the rest beside the tokens.
            let body = json!({"user": {"object": "user", "id": OWNER}, "organization_id": null, "access_token": access, "refresh_token": refresh, "authentication_method": "GoogleOAuth"});
            answer(stream, "200 OK", &body.to_string());
        }
        Reply::Raw(status, body) => answer(stream, status, &body),
    }
}

fn device_code(reply: &Reply) -> Option<String> {
    let Reply::Raw(_, body) = reply else { return None };
    let value: Value = serde_json::from_str(body).ok()?;
    Some(value.get("device_code")?.as_str()?.to_string())
}

fn seen(content_type: &str, body: &[u8]) -> Seen {
    let raw = String::from_utf8_lossy(body).into_owned();
    let form = raw.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect();
    Seen { at: Instant::now(), content_type: content_type.to_string(), raw, form }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// Form-encoded, and a public client's: its id and no secret, the way WorkOS
/// documents the device flow. A redirect URI has no place in it.
fn is_public_form(seen: &Seen) -> bool {
    seen.content_type.starts_with("application/x-www-form-urlencoded")
        && seen.form.get("client_id").map(String::as_str) == Some(CLIENT)
        && !seen.form.contains_key("client_secret")
        && !seen.form.contains_key("redirect_uri")
}

fn serve(mut stream: TcpStream, base: &str, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut length = 0usize;
    let mut content_type = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-type:") {
            content_type = value.trim().to_string();
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
    } else if path == "/user_management/authorize/device" {
        let seen = seen(&content_type, &body);
        let mut state = state.lock().unwrap();
        state.seen_authorize.push(seen.clone());
        let reply = state.authorize.clone();
        drop(state);
        match reply {
            Some(reply) if is_public_form(&seen) => send(&mut stream, reply),
            Some(_) => answer(&mut stream, "400 Bad Request", r#"{"error":"invalid_request"}"#),
            None => answer(&mut stream, "404 Not Found", "{}"),
        }
    } else if path == "/user_management/authenticate" && content_type.starts_with("application/x-www-form-urlencoded") {
        let seen = seen(&content_type, &body);
        let mut state = state.lock().unwrap();
        state.seen_polls.push(seen.clone());
        let device = state.authorize.as_ref().and_then(device_code);
        let right = is_public_form(&seen)
            && seen.form.get("grant_type").map(String::as_str) == Some("urn:ietf:params:oauth:grant-type:device_code")
            && device.is_some()
            && seen.form.get("device_code") == device.as_ref();
        let reply = match state.polls.len() {
            0 => None,
            1 => state.polls.front().cloned(),
            _ => state.polls.pop_front(),
        };
        drop(state);
        match reply.filter(|_| right) {
            Some(reply) => send(&mut stream, reply),
            None => answer(&mut stream, "400 Bad Request", r#"{"error":"invalid_grant"}"#),
        }
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

/// The refresh path, checked the way WorkOS checks it: a public client, no
/// secret, and a refresh token that works once. The device grant is `serve`'s.
fn authenticate(request: &Value, base: &str, state: &Mutex<State>) -> Option<Value> {
    if request["client_id"] != CLIENT || request.get("client_secret").is_some() {
        return None;
    }
    let mut state = state.lock().unwrap();
    if request["grant_type"] != "refresh_token" || state.refresh.as_deref() != request["refresh_token"].as_str() || state.refresh.is_none() {
        return None;
    }
    state.refreshes += 1;
    let next = format!("refresh-{}", nonce());
    state.refresh = Some(next.clone());
    let now = now();
    let claims = json!({"iss": format!("{base}/user_management/{CLIENT}"), "sub": OWNER, "client_id": CLIENT, "iat": now - 5, "exp": now + state.refreshed_ttl, "jti": nonce()});
    Some(json!({"access_token": sign(keys::KID_1, keys::KEY_1_PEM, &claims), "refresh_token": next, "user": {"id": OWNER}}))
}
