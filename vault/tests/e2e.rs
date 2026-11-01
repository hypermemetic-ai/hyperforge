//! End to end: the real binaries over loopback and the real age CLI. A plain
//! software age identity stands in for the Secure Enclave key. Every test has
//! a private HOME, a clean environment and its own state directory, and every
//! process is stopped by its own handle.

use base64::Engine;
use futures::StreamExt;
use plexus_client::{decode, ClientEvent, MethodRef, Transport, WsTransport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REAL_AGE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.tools/age");
const VAULT: &str = env!("CARGO_BIN_EXE_hyperforge-vault");
const APPROVE: &str = env!("CARGO_BIN_EXE_vault-approve");
const RUN: &str = env!("CARGO_BIN_EXE_vault-run");
const GET: &str = env!("CARGO_BIN_EXE_vault-get");
const ENCRYPT: &str = env!("CARGO_BIN_EXE_vault-encrypt");
const SHELL_WAIT: Duration = Duration::from_secs(20);

fn canary_line() -> String {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom").unwrap().read_exact(&mut bytes).unwrap();
    format!("CANARY-hyperforge-vault-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One test's world.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    bin: PathBuf,
    owner: PathBuf,
    recovery: PathBuf,
    recipients: PathBuf,
    port: u16,
    canary: String,
    value: Vec<u8>,
    n: Mutex<u32>,
}

impl World {
    fn new() -> World {
        assert!(Path::new(REAL_AGE_DIR).join("age").exists(), "run scripts/fetch-age.sh: the tests need the real age CLI");
        let tmp = tempfile::Builder::new().prefix("vault-e2e").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = tmp.path().to_path_buf();
        let (home, state, logs, bin) = (root.join("home"), root.join("state"), root.join("logs"), root.join("bin"));
        for d in [&home, &logs, &bin] {
            fs::create_dir_all(d).unwrap();
        }
        // `age` on PATH logs its argv (so a test can read every argument the
        // tools ever passed it) and execs the real one.
        write_script(&bin.join("age"), &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/age-argv.log'\nexec '{REAL_AGE_DIR}/age' \"$@\"\n", logs.display()));
        // `osascript` records its argv, one argument a line, and does nothing.
        write_script(&bin.join("osascript"), &format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> '{}/osascript-argv.log'\n", logs.display()));
        let owner = root.join("owner.key");
        let recovery = root.join("recovery.key");
        let recipients = root.join("recipients.txt");
        let mut lines = String::new();
        for key in [&owner, &recovery] {
            let out = Command::new(format!("{REAL_AGE_DIR}/age-keygen")).arg("-o").arg(key).output().unwrap();
            assert!(out.status.success());
            let text = String::from_utf8(out.stderr).unwrap();
            lines.push_str(text.trim().strip_prefix("Public key: ").unwrap());
            lines.push('\n');
        }
        fs::write(&recipients, lines).unwrap();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let canary = canary_line();
        let value = format!("{canary}\nsecond line\n").into_bytes();
        World { _tmp: tmp, root, home, state, logs, bin, owner, recovery, recipients, port, canary, value, n: Mutex::new(0) }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    fn log(&self, stem: &str) -> PathBuf {
        let mut n = self.n.lock().unwrap();
        *n += 1;
        self.logs.join(format!("{stem}-{n}.log"))
    }

    fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("VAULT_URL", self.url())
            .env("PLEXUS_NO_REGISTRY", "1")
            .env("PLEXUS_WS_TRACE", "1");
        c
    }

    fn server(&self) -> Server {
        self.server_with(300, 60)
    }

    fn server_with(&self, request_ttl: u64, result_ttl: u64) -> Server {
        let log = fs::File::create(self.log("server")).unwrap();
        let child = self
            .command(VAULT)
            .env("RUST_LOG", "trace")
            .args(["--bind", "127.0.0.1", "--port", &self.port.to_string()])
            .arg("--state-dir")
            .arg(&self.state)
            .args(["--request-ttl-secs", &request_ttl.to_string(), "--result-ttl-secs", &result_ttl.to_string()])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut server = Server { child };
        let addr = format!("127.0.0.1:{}", self.port);
        let start = Instant::now();
        while std::net::TcpStream::connect(&addr).is_err() {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!("the server exited: {status}");
            }
            assert!(start.elapsed() < SHELL_WAIT, "the server did not come up");
            std::thread::sleep(Duration::from_millis(20));
        }
        server
    }

    fn encrypt(&self, name: &str) -> std::process::Output {
        self.encrypt_value(name, &self.value)
    }

    fn encrypt_value(&self, name: &str, value: &[u8]) -> std::process::Output {
        let mut child = self
            .command(ENCRYPT)
            .arg(name)
            .arg(&self.recipients)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(value).unwrap();
        let out = child.wait_with_output().unwrap();
        append_log(&self.log("encrypt"), &out);
        out
    }

    fn approver(&self) -> Approver {
        self.approver_with(&self.owner)
    }

    fn approver_with(&self, identity: &Path) -> Approver {
        self.approver_from(self.command(APPROVE), identity)
    }

    /// An approver without PLEXUS_WS_TRACE, whose output is only what its
    /// owner would read: the trace prints raw frames, by design.
    fn approver_untraced(&self) -> Approver {
        let mut command = self.command(APPROVE);
        command.env_remove("PLEXUS_WS_TRACE");
        self.approver_from(command, &self.owner)
    }

    fn approver_from(&self, mut command: Command, identity: &Path) -> Approver {
        let mut child = command
            .arg("--identity")
            .arg(identity)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let log = self.log("approve");
        for mut stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ] {
            let output = output.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    output.lock().unwrap().extend_from_slice(&chunk[..n]);
                    fs::OpenOptions::new().create(true).append(true).open(&log).unwrap().write_all(&chunk[..n]).unwrap();
                }
            });
        }
        Approver { stdin: child.stdin.take(), child, output, prompts_answered: 0 }
    }

    /// `vault-run secret --purpose … --env MY_SECRET -- sh -c script`, its
    /// output in logs/.
    fn run_cmd(&self, secret: &str, purpose: &str, script: &str) -> Caller {
        let log = fs::File::create(self.log("run")).unwrap();
        let child = self
            .command(RUN)
            .args([secret, "--purpose", purpose, "--env", "MY_SECRET", "--poll-ms", "100", "--", "sh", "-c", script])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Caller { child }
    }

    fn secret_file(&self, name: &str) -> PathBuf {
        self.state.join("secrets").join(format!("{name}.age"))
    }

    /// Writes the value's sha256 to `out`, or nothing if the command never runs.
    fn hash_script(&self, out: &Path) -> String {
        format!("printf %s \"$MY_SECRET\" | sha256sum | cut -d' ' -f1 > '{}'", out.display())
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("audit line is not JSON: {e}")))
            .collect()
    }

    fn audit_events(&self) -> Vec<String> {
        self.audit().iter().map(|e| e["event"].as_str().unwrap().to_string()).collect()
    }

    /// Every form of the canary, in every file under `dirs`.
    fn leaks(&self, dirs: &[&Path]) -> Vec<String> {
        let needles = needles(&self.canary);
        let mut hits = Vec::new();
        for dir in dirs {
            scan(dir, &needles, &mut hits);
        }
        hits
    }

    fn assert_no_leak_anywhere(&self) {
        let hits = self.leaks(&[&self.state, &self.logs, &self.home]);
        assert!(hits.is_empty(), "canary found: {hits:?}");
    }
}

fn needles(canary: &str) -> Vec<Vec<u8>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let hex: String = canary.bytes().map(|b| format!("{b:02x}")).collect();
    // The body alone, and its encodings: a fragment is as much a leak as the whole.
    vec![canary.as_bytes().to_vec(), b64.encode(canary).into_bytes(), hex.into_bytes()]
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn scan(path: &Path, needles: &[Vec<u8>], hits: &mut Vec<String>) {
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            scan(&entry.unwrap().path(), needles, hits);
        }
    } else if meta.is_file() {
        let bytes = fs::read(path).unwrap();
        if needles.iter().any(|n| contains(&bytes, n)) {
            hits.push(path.display().to_string());
        }
    }
}

fn write_script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn append_log(path: &Path, out: &std::process::Output) {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    f.write_all(&out.stdout).unwrap();
    f.write_all(&out.stderr).unwrap();
}

struct Server {
    child: Child,
}

impl Server {
    /// SIGKILL: nothing gets to flush on the way out.
    fn crash(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Approver {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
    prompts_answered: usize,
}

const PROMPT: &str = "Approve? [y/N]";

impl Approver {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Waits for the next prompt, answers it.
    fn answer(&mut self, line: &str) {
        let want = self.prompts_answered + 1;
        wait_until("a prompt", SHELL_WAIT, || self.text().matches(PROMPT).count() >= want);
        self.prompts_answered = want;
        writeln!(self.stdin.as_mut().unwrap(), "{line}").unwrap();
    }
}

impl Drop for Approver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Caller {
    child: Child,
}

impl Caller {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < SHELL_WAIT, "the caller did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One raw RPC over the same WebSocket client the tools use: the data events.
fn rpc(url: &str, method: &'static str, params: Value) -> Result<Vec<Value>, String> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let t = WsTransport::connect(url, "hyperforge-vault").await.map_err(|e| e.to_string())?;
        let mut items = decode::<Value>(t.call(MethodRef::new("vault", method), params));
        let mut out = Vec::new();
        while let Some(item) = items.next().await {
            match item.map_err(|e| e.to_string())? {
                ClientEvent::Data(v) => out.push(v),
                _ => {}
            }
        }
        Ok(out)
    })
}

/// The call's only event, whatever it is.
fn rpc1(w: &World, method: &'static str, params: Value) -> Value {
    let mut events = rpc(&w.url(), method, params).unwrap();
    assert_eq!(events.len(), 1, "{method}: {events:?}");
    events.pop().unwrap()
}

fn is_error(v: &Value) -> bool {
    v["type"] == "error"
}

fn new_key() -> (age::x25519::Identity, String) {
    let id = age::x25519::Identity::generate();
    let public = id.to_public().to_string();
    (id, public)
}

fn request_raw(w: &World, secret: &str, purpose: &str, public: &str) -> String {
    let v = rpc1(w, "request", json!({"secret": secret, "purpose": purpose, "one_time_pubkey": public, "caller": "test"}));
    assert_eq!(v["type"], "requested", "{v}");
    v["id"].as_str().unwrap().to_string()
}

fn open(id: &age::x25519::Identity, sealed: &str) -> Result<Vec<u8>, String> {
    let reader = age::armor::ArmoredReader::new(sealed.as_bytes());
    let d = age::Decryptor::new_buffered(reader).map_err(|e| e.to_string())?;
    let mut r = d.decrypt(std::iter::once(id as &dyn age::Identity)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    r.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// A sealed blob for `public`, made with the real CLI.
fn seal_for(public: &str, plaintext: &[u8]) -> String {
    let mut child = Command::new(format!("{REAL_AGE_DIR}/age"))
        .args(["-a", "-r", public])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(plaintext).unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
}

// ---- the cases ---------------------------------------------------------

#[test]
fn happy_path_end_to_end() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("db-password").status.success());
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("db-password", "nightly backup", &w.hash_script(&out));
    approver.answer("y");
    let status = caller.wait();
    assert!(status.success(), "{status}");
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));

    let shown = approver.text();
    for field in ["db-password", "nightly backup"] {
        assert!(shown.contains(field), "approver must show {field}: {shown}");
    }
    assert!(shown.contains("caller"), "approver must show the caller: {shown}");
    assert!(shown.contains("time"), "approver must show the time: {shown}");
    let notified = fs::read_to_string(w.logs.join("osascript-argv.log")).unwrap();
    assert!(notified.contains("db-password"), "{notified}");
    // Decrypted with the identity, resealed to the one-time key, through age.
    let argv = fs::read_to_string(w.logs.join("age-argv.log")).unwrap();
    assert!(argv.contains(&format!("-d -i {}", w.owner.display())), "{argv}");
    assert!(argv.contains("-r age1"), "{argv}");
    w.assert_no_leak_anywhere();
}

#[test]
fn no_approval_means_no_secret() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "unanswered", &w.hash_script(&out));
    wait_until("a prompt", SHELL_WAIT, || approver.text().contains(PROMPT));
    std::thread::sleep(Duration::from_secs(2));
    assert!(caller.child.try_wait().unwrap().is_none(), "the caller must still be waiting");
    assert!(!out.exists(), "the command ran without approval");
    let audit = w.audit_events();
    assert!(audit.iter().all(|e| e != "fulfill" && e != "deliver"), "{audit:?}");
    // And with no approver at all: the request sits pending.
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "nobody home", &public);
    drop(approver);
    let v = rpc1(&w, "result", json!({"id": id}));
    assert_eq!(v["status"], "pending", "{v}");
    assert!(v["sealed"].is_null(), "{v}");
}

#[test]
fn denied_request_gives_nothing() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "please", &w.hash_script(&out));
    approver.answer("n");
    assert!(!caller.wait().success());
    assert!(!out.exists());
    assert!(w.audit_events().iter().any(|e| e == "deny"));
    // Denying never ran age at all.
    assert!(!fs::read_to_string(w.logs.join("age-argv.log")).unwrap_or_default().contains("-d "));
}

#[test]
fn anything_but_yes_is_a_denial() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "please", &w.hash_script(&out));
    approver.answer("");
    assert!(!caller.wait().success());
    assert!(!out.exists());
}

#[test]
fn an_expired_request_gives_nothing() {
    let w = World::new();
    let _server = w.server_with(2, 60);
    assert!(w.encrypt("s").status.success());
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "too slow", &w.hash_script(&out));
    assert!(!caller.wait().success());
    assert!(!out.exists());
    assert!(w.audit_events().iter().any(|e| e == "expire"), "{:?}", w.audit_events());
    assert!(rpc(&w.url(), "pending", json!({})).unwrap().is_empty());
}

#[test]
fn a_result_reads_once() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (key, public) = new_key();
    let id = request_raw(&w, "s", "twice", &public);
    let mut approver = w.approver();
    approver.answer("y");
    wait_until("fulfil", SHELL_WAIT, || w.audit_events().iter().any(|e| e == "fulfill"));
    let first = rpc1(&w, "result", json!({"id": id}));
    assert_eq!(first["status"], "sealed", "{first}");
    assert_eq!(open(&key, first["sealed"].as_str().unwrap()).unwrap(), w.value);
    let second = rpc1(&w, "result", json!({"id": id}));
    assert!(is_error(&second), "{second}");
    assert!(!fs::read_to_string(w.state.join("requests").join(format!("{id}.json"))).unwrap().contains("AGE ENCRYPTED"));
}

#[test]
fn an_unread_result_is_deleted_at_expiry() {
    let w = World::new();
    let _server = w.server_with(300, 1);
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "abandoned", &public);
    let mut approver = w.approver();
    approver.answer("y");
    wait_until("fulfil", SHELL_WAIT, || w.audit_events().iter().any(|e| e == "fulfill"));
    let file = w.state.join("requests").join(format!("{id}.json"));
    wait_until("the sealed result to go", SHELL_WAIT, || !fs::read_to_string(&file).unwrap_or_default().contains("AGE ENCRYPTED"));
    let v = rpc1(&w, "result", json!({"id": id}));
    assert_eq!(v["status"], "expired", "{v}");
    assert!(v["sealed"].is_null());
}

/// All of it, on one run: files, logs, the frames on the wire (the check that
/// needs no access to memory: what the server was ever sent), the server's
/// process image, and that the server has no key to decrypt anything with.
#[test]
fn the_server_holds_no_plaintext() {
    let w = World::new();
    let mut server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "audit me", &w.hash_script(&out));
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
    // A second request, left pending, so a sealed-less record is on disk too.
    let (_, public) = new_key();
    request_raw(&w, "s", "pending at scan", &public);

    // Positive control: the scan finds a planted canary in every encoding.
    let control = w.root.join("control");
    fs::create_dir_all(&control).unwrap();
    for (i, needle) in needles(&w.canary).iter().enumerate() {
        fs::write(control.join(format!("{i}")), needle).unwrap();
    }
    assert_eq!(w.leaks(&[&control]).len(), 3);
    fs::remove_dir_all(&control).unwrap();

    assert_eq!(w.leaks(&[&w.state, &w.logs]), Vec::<String>::new());

    // The wire: the caller's and the approver's traces hold every frame
    // either exchanged with the server.
    let mut frames = String::new();
    for entry in fs::read_dir(&w.logs).unwrap() {
        frames.push_str(&String::from_utf8_lossy(&fs::read(entry.unwrap().path()).unwrap()));
    }
    for method in ["vault.request", "vault.get_request", "vault.fulfill", "vault.result", "vault.put"] {
        assert!(frames.contains(method), "the trace must hold {method}: tracing is off?");
    }
    assert!(!frames.contains(&w.canary));

    // No key material of any kind in the state.
    let keys = {
        let mut hits = Vec::new();
        scan(&w.state, &[b"AGE-SECRET-KEY".to_vec()], &mut hits);
        hits
    };
    assert_eq!(keys, Vec::<String>::new());
    // The server's source reaches for no decryption.
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
    for forbidden in [".decrypt(", "x25519::Identity", "age::Identity"] {
        assert!(!source.contains(forbidden), "server.rs mentions {forbidden}");
    }

    // The process image, for what it is worth.
    assert!(!memory_contains(server.child.id(), w.canary.as_bytes()), "canary in the server's memory");
    assert!(memory_contains(server.child.id(), b"age-encryption.org"), "control: the scan reads its memory");
    server.child.kill().unwrap();
}

fn memory_contains(pid: u32, needle: &[u8]) -> bool {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
    let mut mem = fs::File::open(format!("/proc/{pid}/mem")).unwrap();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let (range, perms) = (fields.next().unwrap(), fields.next().unwrap());
        if !perms.starts_with('r') {
            continue;
        }
        let (start, end) = range.split_once('-').unwrap();
        let (start, end) = (u64::from_str_radix(start, 16).unwrap(), u64::from_str_radix(end, 16).unwrap());
        if end - start > 1 << 30 || mem.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut region = vec![0u8; (end - start) as usize];
        if mem.read_exact(&mut region).is_ok() && contains(&region, needle) {
            return true;
        }
    }
    false
}

#[test]
fn a_wrong_one_time_key_cannot_open_a_result() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (right, public) = new_key();
    let (wrong, _) = new_key();
    let id = request_raw(&w, "s", "keys", &public);
    let mut approver = w.approver();
    approver.answer("y");
    wait_until("fulfil", SHELL_WAIT, || w.audit_events().iter().any(|e| e == "fulfill"));
    let v = rpc1(&w, "result", json!({"id": id}));
    let sealed = v["sealed"].as_str().unwrap();
    assert!(open(&wrong, sealed).is_err());
    assert_eq!(open(&right, sealed).unwrap(), w.value);
    // Nor do the owner's or recovery keys: the seal is to the one-time key alone.
    for key in [&w.owner, &w.recovery] {
        let mut child = Command::new(format!("{REAL_AGE_DIR}/age"))
            .arg("-d").arg("-i").arg(key)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(sealed.as_bytes()).unwrap();
        assert!(!child.wait_with_output().unwrap().status.success());
    }
}

#[test]
fn fulfill_is_refused_for_an_unknown_or_expired_id() {
    let w = World::new();
    let _server = w.server_with(1, 60);
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let sealed = seal_for(&public, b"not the value");
    let v = rpc1(&w, "fulfill", json!({"id": "no-such-id", "sealed": sealed}));
    assert!(is_error(&v), "{v}");
    let id = request_raw(&w, "s", "expires", &public);
    std::thread::sleep(Duration::from_millis(1500));
    let v = rpc1(&w, "fulfill", json!({"id": id, "sealed": sealed}));
    assert!(is_error(&v), "{v}");
    assert!(!w.audit_events().iter().any(|e| e == "fulfill"));
    // Nor is what is not age format accepted, on a live request.
    let w2 = World::new();
    let _server2 = w2.server();
    assert!(w2.encrypt("s").status.success());
    let id = request_raw(&w2, "s", "garbage", &public);
    let v = rpc1(&w2, "fulfill", json!({"id": id, "sealed": "hello"}));
    assert!(is_error(&v), "{v}");
}

#[test]
fn a_restart_keeps_pending_requests() {
    let w = World::new();
    let server = w.server();
    assert!(w.encrypt("s").status.success());
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "survive", &w.hash_script(&out));
    wait_until("the request", SHELL_WAIT, || rpc(&w.url(), "pending", json!({})).map(|p| p.len() == 1).unwrap_or(false));
    server.crash();
    let _server = w.server();
    let pending = rpc(&w.url(), "pending", json!({})).unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["purpose"], "survive");
    let mut approver = w.approver();
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
}

#[test]
fn a_request_that_lapsed_while_down_is_expired_after_restart() {
    let w = World::new();
    let server = w.server_with(1, 60);
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "lapse", &public);
    server.crash();
    std::thread::sleep(Duration::from_millis(1500));
    let _server = w.server_with(1, 60);
    assert!(rpc(&w.url(), "pending", json!({})).unwrap().is_empty());
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "expired");
}

#[test]
fn vault_run_keeps_the_value_off_argv_and_out_of_logs() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();
    let ready = w.root.join("ready");
    // The `:` keeps sh alive: a shell execs its last command in place, and then
    // the cmdline below would be sleep's, not the script.
    let script = format!("echo ok > '{}'; sleep 2; :", ready.display());
    let mut caller = w.run_cmd("s", "argv", &script);
    let pid = caller.pid();
    let needle = w.canary.as_bytes();

    // While it waits, then once it has exec'd (the same pid), and the child's.
    let cmdline = |p: u32| fs::read(format!("/proc/{p}/cmdline")).unwrap_or_default();
    assert!(!contains(&cmdline(pid), needle));
    approver.answer("y");
    wait_until("the command", SHELL_WAIT, || ready.exists());
    let exec_line = cmdline(pid);
    assert!(contains(&exec_line, b"sleep 2"), "pid {pid} must now be the command: {exec_line:?}");
    assert!(!contains(&exec_line, needle), "the value is on argv");
    // Control: the value did reach it, in the environment, which is where it
    // belongs and which is why a same-user reader can see it (README).
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(contains(&environ, needle), "the command must have the value in its environment");
    assert!(contains(&environ, b"MY_SECRET="));
    assert!(caller.wait().success());

    // Every argv age ever had, and every log.
    let argv = fs::read(w.logs.join("age-argv.log")).unwrap();
    assert!(!contains(&argv, needle));
    w.assert_no_leak_anywhere();
}

#[test]
fn recovery_with_the_offline_key_decrypts_a_stored_file() {
    let w = World::new();
    let server = w.server();
    assert!(w.encrypt("s").status.success());
    drop(server);
    let file = w.secret_file("s");
    let out = Command::new(format!("{REAL_AGE_DIR}/age")).arg("-d").arg("-i").arg(&w.recovery).arg(&file).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, w.value);
    // And the stranger's key does not.
    let (stranger, _) = new_key();
    let key = w.root.join("stranger.key");
    use age::secrecy::ExposeSecret;
    fs::write(&key, stranger.to_string().expose_secret()).unwrap();
    let out = Command::new(format!("{REAL_AGE_DIR}/age")).arg("-d").arg("-i").arg(&key).arg(&file).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn the_audit_log_has_every_event_and_no_value() {
    let w = World::new();
    let _server = w.server_with(2, 60);
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();

    let mut ok = w.run_cmd("s", "audit-ok", "true");
    approver.answer("y");
    assert!(ok.wait().success());
    let mut no = w.run_cmd("s", "audit-no", "true");
    approver.answer("n");
    assert!(!no.wait().success());
    drop(approver);
    let mut late = w.run_cmd("s", "audit-late", "true");
    assert!(!late.wait().success());

    let before = fs::read(w.state.join("audit.jsonl")).unwrap();
    let entries = w.audit();
    let events = w.audit_events();
    for wanted in ["request", "approve", "deny", "fulfill", "deliver", "expire"] {
        assert!(events.iter().any(|e| e == wanted), "missing {wanted} in {events:?}");
    }
    for e in &entries {
        assert!(e["ts"].as_u64().unwrap() > 1_700_000_000_000, "{e}");
        if e["event"] != "put" {
            assert_eq!(e["secret"], "s", "{e}");
            assert!(e["purpose"].as_str().unwrap().starts_with("audit-"), "{e}");
            assert!(e["caller"].as_str().is_some(), "{e}");
        }
    }
    let text = String::from_utf8_lossy(&before);
    assert!(!text.contains(&w.canary));
    assert!(!text.contains("AGE ENCRYPTED"), "ciphertext in the audit log");
    // vault.audit says the same, and the log only grows.
    let listed = rpc(&w.url(), "audit", json!({})).unwrap();
    assert_eq!(listed.len(), entries.len());
    rpc(&w.url(), "list", json!({})).unwrap();
    assert!(w.encrypt_value("s2", b"more").status.success());
    let after = fs::read(w.state.join("audit.jsonl")).unwrap();
    assert!(after.len() > before.len() && after.starts_with(&before));
}

#[test]
fn put_refuses_what_is_not_age_a_bad_name_and_an_overwrite() {
    let w = World::new();
    let _server = w.server();
    let (_, public) = new_key();
    let good = seal_for(&public, b"x");
    for bad in ["my password is hunter2", "", "age-encryption.org/v1\n-> nonsense", "-----BEGIN AGE ENCRYPTED FILE-----\nzz\n-----END AGE ENCRYPTED FILE-----\n"] {
        let v = rpc1(&w, "put", json!({"name": "s", "ciphertext": bad}));
        assert!(is_error(&v), "accepted {bad:?}: {v}");
    }
    for name in ["../escape", "a/b", ".hidden", "", "a\nb", "x y"] {
        let v = rpc1(&w, "put", json!({"name": name, "ciphertext": good}));
        assert!(is_error(&v), "accepted name {name:?}: {v}");
    }
    assert!(!w.root.join("escape.age").exists());
    assert_eq!(rpc1(&w, "put", json!({"name": "s", "ciphertext": good}))["type"], "stored");
    let v = rpc1(&w, "put", json!({"name": "s", "ciphertext": good}));
    assert!(is_error(&v), "an overwrite must be refused: {v}");
    let listed = rpc(&w.url(), "list", json!({})).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["name"], "s");
}

#[test]
fn request_refuses_an_unknown_secret_and_a_key_that_is_not_x25519() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let base = |secret: &str, purpose: &str, key: &str| json!({"secret": secret, "purpose": purpose, "one_time_pubkey": key});
    assert!(is_error(&rpc1(&w, "request", base("nope", "p", &public))));
    assert!(is_error(&rpc1(&w, "request", base("s", "", &public))));
    for key in ["-o/tmp/x", "age1notakey", ""] {
        assert!(is_error(&rpc1(&w, "request", base("s", "p", key))), "{key}");
    }
}

/// The caller writes the purpose, the approver renders it, and AppleScript is
/// a language: it must arrive as an argument, and the terminal must not obey it.
#[test]
fn a_hostile_purpose_stays_text() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let pwned = w.root.join("pwned");
    let purpose = format!("safe\x1b[2J\x1b]0;spoof\x07\" & do shell script \"touch {}\" & \"\nsecret: other\u{202e}", pwned.display());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", &purpose, &public);
    let mut approver = w.approver_untraced();
    approver.answer("n");
    wait_until("the denial", SHELL_WAIT, || w.audit_events().iter().any(|e| e == "deny"));
    let shown = approver.text();
    assert!(!shown.contains('\x1b') && !shown.contains('\x07') && !shown.contains('\u{202e}'), "{shown:?}");
    assert!(!shown.contains("\nsecret: other"), "the purpose forged a line: {shown:?}");
    // The script is a constant; the text is argv.
    let log = fs::read_to_string(w.logs.join("osascript-argv.log")).unwrap();
    let args: Vec<&str> = log.lines().collect();
    let scripts: Vec<&str> = args.windows(2).filter(|p| p[0] == "-e").map(|p| p[1]).collect();
    assert!(!scripts.is_empty(), "{args:?}");
    assert!(scripts.iter().all(|s| !s.contains("do shell script")), "the purpose reached the script: {scripts:?}");
    assert!(args.iter().any(|a| !scripts.contains(a) && a.contains("do shell script")), "the purpose must be an argument: {args:?}");
    assert!(!pwned.exists());
    let _ = id;
}

#[test]
fn a_failed_decrypt_is_a_denial_not_an_empty_value() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let wrong = w.root.join("wrong.key");
    assert!(Command::new(format!("{REAL_AGE_DIR}/age-keygen")).arg("-o").arg(&wrong).output().unwrap().status.success());
    let mut approver = w.approver_with(&wrong);
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "touch id cancelled", &w.hash_script(&out));
    approver.answer("y");
    assert!(!caller.wait().success());
    assert!(!out.exists(), "the command ran on an empty value");
    let events = w.audit_events();
    assert!(!events.iter().any(|e| e == "fulfill"), "{events:?}");
    assert!(events.iter().any(|e| e == "deny"), "{events:?}");
}

#[test]
fn vault_get_prints_to_a_pipe_and_refuses_a_terminal() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();

    // Piped stdout: the value, exactly.
    let child = w.command(GET).args(["s", "--purpose", "pipe", "--poll-ms", "100"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    approver.answer("y");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, w.value);
    // (Not via `append_log`: this output is the value.)

    // A terminal: refused before any request is made.
    let requests_before = w.audit_events().iter().filter(|e| *e == "request").count();
    let py = "import pty,sys; sys.exit(pty.spawn(sys.argv[1:]) >> 8)";
    let out = w.command("/usr/bin/python3").args(["-I", "-c", py, GET, "s", "--purpose", "tty"]).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("terminal"), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(w.audit_events().iter().filter(|e| *e == "request").count(), requests_before);

    // Unless asked to.
    let py_child = w.command("/usr/bin/python3").args(["-I", "-c", py, GET, "s", "--purpose", "tty2", "--poll-ms", "100", "--allow-terminal"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    approver.answer("y");
    let out = py_child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(contains(&out.stdout, w.canary.as_bytes()));
}

#[test]
fn a_browser_page_cannot_open_the_socket() {
    let w = World::new();
    let _server = w.server();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = |origin: &str| format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{origin}\r\n");
    s.write_all(request("Origin: https://evil.example\r\n").as_bytes()).unwrap();
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).unwrap();
    assert!(!String::from_utf8_lossy(&buf[..n]).contains("101"), "{}", String::from_utf8_lossy(&buf[..n]));
    // Control: the same upgrade with no Origin is what the tools send.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request("").as_bytes()).unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("101"), "{}", String::from_utf8_lossy(&buf[..n]));
}
