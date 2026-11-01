//! `vault-login` against the fake WorkOS: the test plays the browser.

#[path = "support/idp.rs"]
mod idp;

use idp::{Idp, CLIENT};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const LOGIN: &str = env!("CARGO_BIN_EXE_vault-login");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The child, its URL, and its stdout, held open: it prints once more.
fn spawn(idp: &Idp, file: &Path, port: u16) -> (Child, String, impl Sized) {
    let mut child = Command::new(LOGIN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("VAULT_WORKOS_CLIENT_ID", CLIENT)
        .env("VAULT_WORKOS_BASE", &idp.base)
        .env("VAULT_LOGIN_FILE", file)
        .args(["--no-browser", "--redirect-uri", &format!("http://127.0.0.1:{port}/callback")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let url = lines.find_map(|l| l.ok().filter(|l| l.starts_with("http"))).expect("no sign-in URL");
    (child, url, lines)
}

fn query(url: &str, name: &str) -> String {
    let (_, query) = url.split_once('?').unwrap();
    let raw = query.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}="))).unwrap_or_else(|| panic!("no {name} in {url}"));
    raw.replace("%3A", ":").replace("%2F", "/")
}

fn browse(port: u16, path: &str) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    text
}

#[test]
fn the_flow_is_pkce_with_no_secret_and_stores_owner_only() {
    let idp = Idp::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("login.json");
    let port = free_port();
    let (mut child, url, _stdout) = spawn(&idp, &file, port);
    assert!(url.starts_with(&format!("{}/user_management/authorize?", idp.base)), "{url}");
    assert_eq!(query(&url, "response_type"), "code");
    assert_eq!(query(&url, "client_id"), CLIENT);
    assert_eq!(query(&url, "provider"), "authkit");
    assert_eq!(query(&url, "code_challenge_method"), "S256");
    assert_eq!(query(&url, "redirect_uri"), format!("http://127.0.0.1:{port}/callback"));
    let (state, challenge) = (query(&url, "state"), query(&url, "code_challenge"));
    assert!(!url.contains("secret"));

    // Another local page, guessing: told no, and the flow keeps waiting.
    assert!(browse(port, "/callback?code=stolen&state=guess").starts_with("HTTP/1.1 400"));
    assert!(browse(port, &format!("/elsewhere?code=stolen&state={state}")).starts_with("HTTP/1.1 400"));
    assert!(child.try_wait().unwrap().is_none());

    // The browser comes back.
    let code = idp.issue_code(&challenge);
    assert!(browse(port, &format!("/callback?code={code}&state={state}")).starts_with("HTTP/1.1 200"));
    let status = child.wait().unwrap();
    assert!(status.success());

    let stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(stored["access_token"].is_string());
    assert!(stored["refresh_token"].is_string());
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn a_wrong_code_stores_nothing() {
    let idp = Idp::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("login.json");
    let port = free_port();
    let (mut child, url, _stdout) = spawn(&idp, &file, port);
    let state = query(&url, "state");
    idp.issue_code("a challenge for some other verifier");
    browse(port, &format!("/callback?code=wrong&state={state}"));
    assert!(!child.wait().unwrap().success());
    assert!(!file.exists());
}

#[test]
fn a_refused_login_from_workos_ends_the_flow() {
    let idp = Idp::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("login.json");
    let port = free_port();
    let (mut child, url, _stdout) = spawn(&idp, &file, port);
    let state = query(&url, "state");
    browse(port, &format!("/callback?error=access_denied&state={state}"));
    assert!(!child.wait().unwrap().success());
    assert!(!file.exists());
}

/// The refresh token is on this process's heap: no core of it, and no reading it
/// through `/proc` by this user's other processes.
#[cfg(target_os = "linux")]
#[test]
fn the_login_tool_cannot_be_dumped() {
    let idp = Idp::start();
    let dir = tempfile::tempdir().unwrap();
    let (mut child, _url, _stdout) = spawn(&idp, &dir.path().join("login.json"), free_port());
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", child.id())).unwrap();
    let core = limits.lines().find(|l| l.starts_with("Max core file size")).unwrap();
    assert_eq!(core.split_whitespace().nth(4), Some("0"), "{core}");
    let denied = std::fs::File::open(format!("/proc/{}/mem", child.id())).unwrap_err();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied, "dumpable: {denied}");
    child.kill().unwrap();
    child.wait().unwrap();
}
