//! HYPERFORGE-65: the daemon's door. A browser applies no same-origin rule to
//! a WebSocket, so any page open on this machine could open
//! `ws://127.0.0.1:<port>/` and call every method; and a name an attacker
//! points at 127.0.0.1 (DNS rebinding) reaches it too unless `Host` is
//! checked. The daemon names an origin no browser sends and the loopback
//! names of its port: an upgrade from a page (`Origin` set) is refused 403,
//! one naming a foreign host 421; synapse, which sends no `Origin` and a
//! loopback `Host`, is admitted and answered.
//!
//! Every test starts the real binary (`HF_BIN` names another, e.g. the
//! release build) on a free port, never 44104.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

struct Daemon {
    child: Child,
    port: u16,
    _tmp: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon_bin() -> PathBuf {
    std::env::var_os("HF_BIN").map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hyperforge")), PathBuf::from)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    l.local_addr().unwrap().port()
}

/// The daemon on a free port, once it is listening. A port the kernel gave
/// out is free again the moment the probe closes it, so another process may
/// take it first: then the daemon exits naming the port, and it is started
/// again on another — never asked of whatever else answers there.
fn start_daemon() -> Daemon {
    for _ in 0..5 {
        if let Some(d) = try_start_daemon() {
            return d;
        }
    }
    panic!("the daemon did not start on any of five free ports");
}

fn try_start_daemon() -> Option<Daemon> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let port = free_port();
    assert_ne!(port, 44104, "never the installed daemon's port");
    let child = Command::new(daemon_bin())
        .arg("--port")
        .arg(port.to_string())
        .arg("--config-dir")
        .arg(root.join("config"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("XDG_STATE_HOME", root.join("home/.state"))
        .spawn()
        .unwrap();
    let mut d = Daemon { child, port, _tmp: tmp };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if d.child.try_wait().unwrap().is_some() {
            return None;
        }
        // An answered upgrade, not a bare connect: a connect to a port in
        // the ephemeral range can meet itself (TCP simultaneous open) before
        // anything listens there.
        if status_of(&upgrade(port, &format!("127.0.0.1:{port}"), None)) == 101 {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not listen on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Answering, and still ours.
    d.child.try_wait().unwrap().is_none().then_some(d)
}

/// A WebSocket upgrade with exactly these `Host` and `Origin` headers; the
/// status line answered.
fn upgrade(port: u16, host: &str, origin: Option<&str>) -> String {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    );
    if let Some(o) = origin {
        req.push_str(&format!("Origin: {o}\r\n"));
    }
    req.push_str("\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while !buf.windows(2).any(|w| w == b"\r\n") {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    text.lines().next().unwrap_or("").to_string()
}

fn status_of(line: &str) -> u16 {
    line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

#[test]
fn an_upgrade_from_a_page_is_refused() {
    let d = start_daemon();
    let host = format!("127.0.0.1:{}", d.port);
    let line = upgrade(d.port, &host, Some("https://evil.example"));
    assert_eq!(status_of(&line), 403, "Origin https://evil.example, Host {host}: {line}");
    // `null`, from a sandboxed frame or a file:// page, is a page too.
    let line = upgrade(d.port, &host, Some("null"));
    assert_eq!(status_of(&line), 403, "Origin null, Host {host}: {line}");
}

#[test]
fn an_upgrade_naming_a_foreign_host_is_refused() {
    let d = start_daemon();
    let host = format!("attacker.example:{}", d.port);
    let line = upgrade(d.port, &host, None);
    assert_eq!(status_of(&line), 421, "no Origin, Host {host}: {line}");
    // The planner's curl: a page whose name rebinds to 127.0.0.1.
    let line = upgrade(d.port, &host, Some("https://evil.example"));
    assert!(matches!(status_of(&line), 403 | 421), "Origin https://evil.example, Host {host}: {line}");
    // Another port's name is another listener's.
    let host = format!("127.0.0.1:{}", d.port.wrapping_add(1));
    let line = upgrade(d.port, &host, None);
    assert_eq!(status_of(&line), 421, "no Origin, Host {host}: {line}");
}

#[test]
fn an_upgrade_with_no_origin_and_a_loopback_host_is_admitted() {
    let d = start_daemon();
    for host in [format!("127.0.0.1:{}", d.port), format!("localhost:{}", d.port)] {
        let line = upgrade(d.port, &host, None);
        assert_eq!(status_of(&line), 101, "no Origin, Host {host}: {line}");
    }
}

/// `hyperforge status` over a socket that names `name:<port>` as its host.
async fn status_over(name: &str, port: u16) -> Value {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{name}:{port}")).await.unwrap_or_else(|e| panic!("connect {name}:{port}: {e:?}"));
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "lforge-v5.call",
        "params": {"method": "lforge-v5.hyperforge.status", "params": {}},
    });
    ws.send(Message::Text(call.to_string().into())).await.unwrap();
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => panic!("no answer to status over {name}:{port}"),
            m = ws.next() => match m.expect("socket closed").unwrap() {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if t.contains("\"type\":\"status\"") || t.contains("version") {
                        return v;
                    }
                    assert!(v.get("error").is_none(), "status over {name}:{port}: {v}");
                }
                _ => {}
            },
        }
    }
}

#[tokio::test]
async fn a_method_answers_over_loopback_names() {
    let d = start_daemon();
    for name in ["127.0.0.1", "localhost"] {
        let v = status_over(name, d.port).await;
        assert!(v.to_string().contains("version"), "{name}: {v}");
    }
}

fn synapse_bin() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join("synapse")).find(|p| p.is_file())
}

/// Acceptance 3: `synapse -P <port> lforge-v5 hyperforge status`, as it
/// stands, against the binary.
#[test]
fn synapse_status_answers() {
    let Some(synapse) = synapse_bin() else {
        panic!("synapse is not on PATH (the suite wants ~/.local/bin on it)");
    };
    let d = start_daemon();
    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("answer.txt");
    let ran = Command::new(synapse)
        .args(["--no-hold", "-o"])
        .arg(&out)
        .args(["-P", &d.port.to_string(), "lforge-v5", "hyperforge", "status"])
        .env("SYNAPSE_NO_HOLD", "1")
        .env("LC_ALL", "C.UTF-8")
        .output()
        .unwrap();
    let answer = read(&out);
    assert!(
        ran.status.success() && answer.contains("version") && !answer.to_lowercase().contains("error"),
        "exit {:?}\nanswer:\n{answer}\nstderr:\n{}",
        ran.status.code(),
        String::from_utf8_lossy(&ran.stderr)
    );
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}
