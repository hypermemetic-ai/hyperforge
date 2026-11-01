//! End to end: the real binaries over loopback. No age CLI: an `age` on the
//! tools' PATH records that it ran, and [`World::assert_no_age_ran`] fails on
//! it. A plain software age identity stands in for the Secure Enclave key, but for
//! [`a_real_enclave_round_trip`], which is run by hand. Every test has
//! a private HOME, a clean environment and its own state directory, and every
//! process is stopped by its own handle.

#[path = "support/idp.rs"]
mod idp;

use base64::Engine;
use futures::StreamExt;
use plexus_client::{decode, ClientEvent, MethodRef, Transport, WsTransport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    manifest: PathBuf,
    idp: idp::Idp,
    /// The owner's WorkOS access token, as the approver holds it.
    owner_credential: String,
    refresh_token: String,
    /// A machine's token: what `vault-run` and `vault-get` present.
    machine_credential: String,
    port: u16,
    canary: String,
    value: Vec<u8>,
    n: Mutex<u32>,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::Builder::new().prefix("vault-e2e").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = tmp.path().to_path_buf();
        let (home, state, logs, bin) = (root.join("home"), root.join("state"), root.join("logs"), root.join("bin"));
        for d in [&home, &logs, &bin] {
            fs::create_dir_all(d).unwrap();
        }
        // `age` on PATH logs that it ran, argv and all, and fails. Nothing may
        // run it: a tool that did would both fail here and leave this file.
        write_script(&bin.join("age"), &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/age-argv.log'\nexit 1\n", logs.display()));
        // `osascript` records its argv, one argument a line, and does nothing.
        write_script(&bin.join("osascript"), &format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> '{}/osascript-argv.log'\n", logs.display()));
        let owner = root.join("owner.key");
        let recovery = root.join("recovery.key");
        let recipients = root.join("recipients.txt");
        let mut lines = String::new();
        for key in [&owner, &recovery] {
            lines.push_str(&keygen(key));
            lines.push('\n');
        }
        fs::write(&recipients, lines).unwrap();
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let canary = canary_line();
        let value = format!("{canary}\nsecond line\n").into_bytes();
        // Throwaways, this world's alone. The owner's sign-in is what
        // vault-login would have left; the server knows neither its token nor
        // its refresh token. The machine's token reaches it only as a hash.
        let idp = idp::Idp::start();
        let owner_credential = idp.token(idp::OWNER);
        let refresh_token = format!("refresh-{}", canary_line());
        fs::write(root.join("login.json"), json!({"access_token": owner_credential, "refresh_token": refresh_token}).to_string()).unwrap();
        let machine_credential = format!("machine-{}", canary_line());
        fs::write(root.join("machine.credential"), &machine_credential).unwrap();
        fs::write(root.join("machines"), format!("publish {}\n", sha_hex(machine_credential.as_bytes()))).unwrap();
        let manifest = root.join("manifest.json");
        World { _tmp: tmp, root, home, state, logs, bin, owner, recovery, recipients, manifest, idp, owner_credential, refresh_token, machine_credential, port, canary, value, n: Mutex::new(0) }
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
            .env("VAULT_MANIFEST", &self.manifest)
            .env("VAULT_WORKOS_CLIENT_ID", idp::CLIENT)
            .env("VAULT_WORKOS_BASE", &self.idp.base)
            .env("VAULT_LOGIN_FILE", self.root.join("login.json"))
            .env("VAULT_CREDENTIAL_FILE", self.root.join("machine.credential"))
            .env("PLEXUS_NO_REGISTRY", "1")
            .env("PLEXUS_WS_TRACE", "1");
        c
    }

    fn server(&self) -> Server {
        self.server_with(300, 60)
    }

    fn server_with(&self, request_ttl: u64, result_ttl: u64) -> Server {
        let mut command = self.command(VAULT);
        command.env("VAULT_OWNER_USER_ID", idp::OWNER).env("VAULT_MACHINE_CREDENTIALS", self.root.join("machines"));
        self.server_from(command, request_ttl, result_ttl)
    }

    /// A server nobody told who its owner is.
    fn server_without_owner(&self) -> Server {
        self.server_from(self.command(VAULT), 300, 60)
    }

    fn server_from(&self, mut command: Command, request_ttl: u64, result_ttl: u64) -> Server {
        let log = fs::File::create(self.log("server")).unwrap();
        let child = command
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

    /// `vault-approve --once`.
    fn approver_once(&self) -> Approver {
        self.approver_once_with(&self.owner)
    }

    fn approver_once_with(&self, identity: &Path) -> Approver {
        let mut command = self.command(APPROVE);
        command.env_remove("PLEXUS_WS_TRACE").arg("--once");
        self.approver_from(command, identity)
    }

    /// The owner's key behind a FIFO: see [`KeyWitness`].
    fn watched_owner(&self) -> KeyWitness {
        KeyWitness::new(&self.root.join("watched.key"), fs::read(&self.owner).unwrap())
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

    /// The PATH trap never fired: no tool spawned `age`.
    fn assert_no_age_ran(&self) {
        let log = self.logs.join("age-argv.log");
        assert!(!log.exists(), "something ran age: {}", fs::read_to_string(&log).unwrap_or_default());
    }
}

/// A new identity at `path`, from our own binary; its public recipient.
fn keygen(path: &Path) -> String {
    let out = Command::new(APPROVE).env_clear().arg("recovery-keygen").arg("--out").arg(path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Whether the approver ever read its identity, from outside it. The key is
/// behind a FIFO, so the kernel says when someone opened it: a process that
/// never opened the key has decrypted nothing, because it is read at each
/// decrypt and never held. This is what replaces reading `age -d` off an argv.
struct KeyWitness {
    path: PathBuf,
    reads: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl KeyWitness {
    fn new(path: &Path, key: Vec<u8>) -> KeyWitness {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` outlives the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (reads, stop) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (counter, stopped, fifo) = (reads.clone(), stop.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                // Non-blocking, a FIFO opens for write only if a reader is there.
                match fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&fifo) {
                    Ok(mut f) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        f.write_all(&key).unwrap();
                        drop(f);
                        // The reader's gone by the next look.
                        std::thread::sleep(Duration::from_millis(300));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        KeyWitness { path: path.to_path_buf(), reads, stop }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Drop for KeyWitness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
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

    /// Waits for it to exit by itself.
    fn exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < SHELL_WAIT, "the approver did not exit: {}", self.text());
            std::thread::sleep(Duration::from_millis(20));
        }
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
    rpc_as(url, None, method, params)
}

/// [`rpc`], presenting `credential` as a Bearer token on the upgrade.
fn rpc_as(url: &str, credential: Option<&str>, method: &'static str, params: Value) -> Result<Vec<Value>, String> {
    let headers: Vec<(String, String)> = credential.map(|c| ("Authorization".to_string(), format!("Bearer {c}"))).into_iter().collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let t = WsTransport::connect_with_headers(url, "hyperforge-vault", &headers).await.map_err(|e| e.to_string())?;
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

/// [`rpc_as`], as the owner.
fn rpc_owner(w: &World, method: &'static str, params: Value) -> Result<Vec<Value>, String> {
    rpc_as(&w.url(), Some(&w.owner_credential), method, params)
}

/// The call's only event, whatever it is. Every method now wants a credential,
/// so what was an anonymous look is the owner's.
fn rpc1(w: &World, method: &'static str, params: Value) -> Value {
    let mut events = rpc_owner(w, method, params).unwrap();
    assert_eq!(events.len(), 1, "{method}: {events:?}");
    events.pop().unwrap()
}

/// [`rpc1`], as the owner: the call the approver would make.
fn rpc1_owner(w: &World, method: &'static str, params: Value) -> Value {
    rpc1(w, method, params)
}

/// Refused by the socket, or by the method: either way, it did nothing.
fn refused(answer: Result<Vec<Value>, String>) -> bool {
    match answer {
        Err(_) => true,
        Ok(events) => events.len() == 1 && is_error(&events[0]),
    }
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

/// What `age -d -i key` would say: read by the crate's own identity-file parser,
/// which is not the one under test. tests/interop.rs does this with a real CLI.
fn open_with_key_file(key: &Path, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let identities = age::IdentityFile::from_file(key.display().to_string()).map_err(|e| e.to_string())?.into_identities().map_err(|e| e.to_string())?;
    let d = age::Decryptor::new_buffered(age::armor::ArmoredReader::new(ciphertext)).map_err(|e| e.to_string())?;
    let mut r = d.decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    r.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// A sealed blob for `public`.
fn seal_for(public: &str, plaintext: &[u8]) -> String {
    age::encrypt_and_armor(&public.parse::<age::x25519::Recipient>().unwrap(), plaintext).unwrap()
}

// ---- the cases ---------------------------------------------------------

#[test]
fn happy_path_end_to_end() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("db-password").status.success());
    let witness = w.watched_owner();
    let mut approver = w.approver_with(&witness.path);
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
    // Decrypted with the identity, once, resealed to the one-time key (the
    // caller opened it, above, with that key), and without age.
    assert_eq!(witness.reads(), 1);
    w.assert_no_age_ran();
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
    let witness = w.watched_owner();
    let mut approver = w.approver_with(&witness.path);
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "please", &w.hash_script(&out));
    approver.answer("n");
    assert!(!caller.wait().success());
    assert!(!out.exists());
    assert!(w.audit_events().iter().any(|e| e == "deny"));
    // Denying never touched the key at all.
    assert_eq!(witness.reads(), 0);
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
    assert!(rpc_owner(&w, "pending", json!({})).unwrap().is_empty());
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
        assert!(open_with_key_file(key, sealed.as_bytes()).is_err());
    }
}

#[test]
fn fulfill_is_refused_for_an_unknown_or_expired_id() {
    let w = World::new();
    let _server = w.server_with(1, 60);
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let sealed = seal_for(&public, b"not the value");
    let v = rpc1_owner(&w, "fulfill", json!({"id": "no-such-id", "sealed": sealed}));
    assert!(is_error(&v), "{v}");
    let id = request_raw(&w, "s", "expires", &public);
    std::thread::sleep(Duration::from_millis(1500));
    let v = rpc1_owner(&w, "fulfill", json!({"id": id, "sealed": sealed}));
    assert!(is_error(&v), "{v}");
    assert!(!w.audit_events().iter().any(|e| e == "fulfill"));
    // Nor is what is not age format accepted, on a live request.
    let w2 = World::new();
    let _server2 = w2.server();
    assert!(w2.encrypt("s").status.success());
    let id = request_raw(&w2, "s", "garbage", &public);
    let v = rpc1_owner(&w2, "fulfill", json!({"id": id, "sealed": "hello"}));
    assert!(is_error(&v), "{v}");
}

#[test]
fn a_restart_keeps_pending_requests() {
    let w = World::new();
    let server = w.server();
    assert!(w.encrypt("s").status.success());
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "survive", &w.hash_script(&out));
    wait_until("the request", SHELL_WAIT, || rpc_owner(&w, "pending", json!({})).map(|p| p.len() == 1).unwrap_or(false));
    server.crash();
    let _server = w.server();
    let pending = rpc_owner(&w, "pending", json!({})).unwrap();
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
    assert!(rpc_owner(&w, "pending", json!({})).unwrap().is_empty());
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

    // No age had an argv, and every log.
    w.assert_no_age_ran();
    w.assert_no_leak_anywhere();
}

#[test]
fn recovery_with_the_offline_key_decrypts_a_stored_file() {
    let w = World::new();
    let server = w.server();
    assert!(w.encrypt("s").status.success());
    drop(server);
    let file = w.secret_file("s");
    let ciphertext = fs::read(&file).unwrap();
    assert_eq!(open_with_key_file(&w.recovery, &ciphertext).unwrap(), w.value);
    // And the stranger's key does not.
    let (stranger, _) = new_key();
    let key = w.root.join("stranger.key");
    use age::secrecy::ExposeSecret;
    fs::write(&key, stranger.to_string().expose_secret()).unwrap();
    assert!(open_with_key_file(&key, &ciphertext).is_err());
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
    let listed = rpc_owner(&w, "audit", json!({})).unwrap();
    assert_eq!(listed.len(), entries.len());
    rpc_owner(&w, "list", json!({})).unwrap();
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
    let listed = rpc_owner(&w, "list", json!({})).unwrap();
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
    keygen(&wrong);
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
    // With the owner's own token: the origin check has to hold by itself.
    let request = |origin: &str| format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Authorization: Bearer {}\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{origin}\r\n", w.owner_credential);
    s.write_all(request("Origin: https://evil.example\r\n").as_bytes()).unwrap();
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).unwrap();
    assert!(!String::from_utf8_lossy(&buf[..n]).contains("101"), "{}", String::from_utf8_lossy(&buf[..n]));
    // Control: the same upgrade with no Origin is what the tools send.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request("").as_bytes()).unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("101"), "{}", String::from_utf8_lossy(&buf[..n]));
}

// ---- HYPE-16: the manifest, the owner, --once ---------------------------

/// The compromised server: `a` was approved, the bytes served are `b`'s, and
/// both are real ciphertexts to the owner's key.
#[test]
fn a_swapped_ciphertext_is_refused_with_nothing_decrypted() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("a").status.success());
    assert!(w.encrypt_value("b", b"the other secret\n").status.success());
    fs::copy(w.secret_file("b"), w.secret_file("a")).unwrap();
    // Control: what is served under `a` opens with the owner's key, so only a
    // check before the decrypt can stop this.
    assert_eq!(open_with_key_file(&w.owner, &fs::read(w.secret_file("a")).unwrap()).unwrap(), b"the other secret\n");

    let witness = w.watched_owner();
    let mut approver = w.approver_with(&witness.path);
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("a", "swap", &w.hash_script(&out));
    approver.answer("y");
    let status = caller.wait();
    assert!(!out.exists(), "the attack worked: the caller received b under the name a");
    assert!(!status.success());
    assert_eq!(witness.reads(), 0, "the key was read for a ciphertext the manifest does not know");
    let events = w.audit_events();
    assert!(!events.iter().any(|e| e == "fulfill"), "{events:?}");
    assert!(approver.text().contains("manifest"), "the refusal must say why: {}", approver.text());
}

/// A secret the manifest has never heard of: no entry, no decrypt.
#[test]
fn a_secret_outside_the_manifest_is_refused_with_nothing_decrypted() {
    let w = World::new();
    let _server = w.server();
    let (_, public) = new_key();
    let recipients = fs::read_to_string(&w.recipients).unwrap();
    let stored = seal_for(recipients.lines().next().unwrap(), b"stored by someone else\n");
    assert_eq!(rpc1(&w, "put", json!({"name": "stranger", "ciphertext": stored}))["type"], "stored");
    request_raw(&w, "stranger", "unknown", &public);
    let witness = w.watched_owner();
    let mut approver = w.approver_with(&witness.path);
    approver.answer("y");
    wait_until("the refusal", SHELL_WAIT, || w.audit_events().iter().any(|e| e == "deny" || e == "fulfill"));
    assert_eq!(witness.reads(), 0);
    assert!(!w.audit_events().iter().any(|e| e == "fulfill"));
}

/// The rest of what `vault-encrypt` owes the manifest: a refused overwrite
/// leaves the recorded hash alone, so the approved name still opens.
#[test]
fn a_refused_overwrite_leaves_the_manifest_alone() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let before = fs::read(&w.manifest).unwrap();
    assert!(!w.encrypt_value("s", b"a value chosen later\n").status.success());
    assert_eq!(fs::read(&w.manifest).unwrap(), before);
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "still fine", &w.hash_script(&out));
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
}

/// Any socket answering for the owner, with a value it chose.
#[test]
fn a_non_owner_fulfil_is_refused() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "forge me", &w.hash_script(&out));
    wait_until("the request", SHELL_WAIT, || rpc_owner(&w, "pending", json!({})).map(|p| p.len() == 1).unwrap_or(false));
    let pending = rpc_owner(&w, "pending", json!({})).unwrap();
    let (id, public) = (pending[0]["id"].as_str().unwrap(), pending[0]["one_time_pubkey"].as_str().unwrap());
    let forged = seal_for(public, b"a value the attacker chose");
    let answer = rpc(&w.url(), "fulfill", json!({"id": id, "sealed": forged}));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!out.exists(), "the attack worked: the caller ran on a forged value");
    assert!(refused(answer.clone()), "{answer:?}");
    assert!(!w.audit_events().iter().any(|e| e == "fulfill"));
    assert!(caller.child.try_wait().unwrap().is_none(), "the caller must still be waiting");
    // And the owner's own answer still lands.
    let mut approver = w.approver();
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
}

#[test]
fn a_non_owner_get_request_is_refused() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "read me", &public);
    let answer = rpc(&w.url(), "get_request", json!({"id": id}));
    assert!(refused(answer.clone()), "{answer:?}");
    assert_eq!(rpc1_owner(&w, "get_request", json!({"id": id}))["type"], "request");
}

#[test]
fn a_non_owner_deny_is_refused() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "deny me", &public);
    let answer = rpc(&w.url(), "deny", json!({"id": id}));
    assert!(refused(answer.clone()), "{answer:?}");
    assert!(!w.audit_events().iter().any(|e| e == "deny"));
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "pending");
    assert_eq!(rpc1_owner(&w, "deny", json!({"id": id}))["type"], "denied");
}

#[test]
fn a_wrong_credential_is_refused_on_all_three() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "wrong key", &public);
    let sealed = seal_for(&public, b"forged");
    for (method, params) in [
        ("get_request", json!({"id": id})),
        ("fulfill", json!({"id": id, "sealed": sealed})),
        ("deny", json!({"id": id})),
    ] {
        let answer = rpc_as(&w.url(), Some("not-the-credential"), method, params);
        assert!(refused(answer.clone()), "{method}: {answer:?}");
    }
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "pending");
}

/// Fail closed: no configured owner means nobody, whatever they present.
#[test]
fn with_no_owner_configured_all_three_are_refused() {
    let w = World::new();
    // Nobody can store or ask on the server under test, so a server with an
    // owner seeds its state first.
    let seeding = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "ownerless", &public);
    drop(seeding);
    let ownerless = w.server_without_owner();
    let sealed = seal_for(&public, b"forged");
    for credential in [None, Some(w.owner_credential.as_str())] {
        for (method, params) in [
            ("get_request", json!({"id": id})),
            ("fulfill", json!({"id": id, "sealed": sealed})),
            ("deny", json!({"id": id})),
        ] {
            let answer = rpc_as(&w.url(), credential, method, params);
            assert!(refused(answer.clone()), "{method} with {credential:?}: {answer:?}");
        }
    }
    drop(ownerless);
    let _server = w.server();
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "pending");
}

#[test]
fn the_owner_credential_stays_out_of_state_and_logs() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver();
    let mut caller = w.run_cmd("s", "credential", "true");
    approver.answer("y");
    assert!(caller.wait().success());
    let mut hits = Vec::new();
    let signature = w.owner_credential.rsplit('.').next().unwrap();
    for dir in [&w.state, &w.logs, &w.home] {
        for secret in [w.owner_credential.as_str(), signature, w.refresh_token.as_str(), w.machine_credential.as_str()] {
            scan(dir, &needles(secret), &mut hits);
        }
    }
    assert_eq!(hits, Vec::<String>::new());
}

// ---- HYPE-17: a token ends and the approver carries on --------------------

/// A sign-in whose access token has `seconds` left, and a refresh token that
/// mints an hour.
fn short_sign_in(w: &World, seconds: i64) {
    let refresh = format!("refresh-{}", idp::nonce());
    let access = w.idp.token_lasting(idp::OWNER, seconds);
    fs::write(w.root.join("login.json"), json!({"access_token": access, "refresh_token": refresh}).to_string()).unwrap();
    w.idp.accept_refresh(&refresh, 3600);
}

#[test]
fn the_approvers_reconnect_gets_a_fresh_token() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    short_sign_in(&w, 8);
    let mut approver = w.approver();
    wait_until("the watch to lose its token and come back", SHELL_WAIT, || w.idp.refreshes() == 1 && approver.text().matches("watching").count() >= 2);
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "after the token ended", &w.hash_script(&out));
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
    let login: Value = serde_json::from_slice(&fs::read(w.root.join("login.json")).unwrap()).unwrap();
    assert_ne!(login["access_token"], w.owner_credential, "the refreshed sign-in is on disk");
}

/// The socket's token ends with the owner reading the prompt: their answer is
/// not lost with it.
#[test]
fn an_answer_outlives_the_watch_socket() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    short_sign_in(&w, 8);
    let mut approver = w.approver();
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "slow reader", &w.hash_script(&out));
    wait_until("the prompt", SHELL_WAIT, || approver.text().contains(PROMPT));
    std::thread::sleep(Duration::from_secs(9));
    approver.answer("y");
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
}

/// The contract: when the session ends, `--once` does not reconnect.
#[test]
fn once_exits_nonzero_when_its_session_ends() {
    let w = World::new();
    let _server = w.server();
    short_sign_in(&w, 7);
    let mut approver = w.approver_once();
    assert!(!approver.exit().success());
    assert_eq!(w.idp.refreshes(), 0, "--once does not refresh and carry on");
}

/// A sign-in that has gone bad says what to do, and the server stays shut.
#[test]
fn a_dead_sign_in_says_run_vault_login() {
    let w = World::new();
    let _server = w.server();
    fs::write(w.root.join("login.json"), json!({"access_token": w.idp.token_lasting(idp::OWNER, -1), "refresh_token": "revoked"}).to_string()).unwrap();
    let mut approver = w.approver_once();
    assert!(!approver.exit().success());
    assert!(approver.text().contains("vault-login"), "{}", approver.text());
}

#[test]
fn once_handles_one_request_and_exits() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let out = w.root.join("hash");
    let mut caller = w.run_cmd("s", "just this one", &w.hash_script(&out));
    wait_until("the request", SHELL_WAIT, || rpc_owner(&w, "pending", json!({})).map(|p| p.len() == 1).unwrap_or(false));
    let (_, public) = new_key();
    let second = request_raw(&w, "s", "not this one", &public);

    let mut approver = w.approver_once();
    approver.answer("y");
    let status = approver.exit();
    assert!(status.success(), "{status}: {}", approver.text());
    assert!(caller.wait().success());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), sha_hex(&w.value));
    assert_eq!(approver.text().matches(PROMPT).count(), 1, "{}", approver.text());
    // The second is left for the next run.
    assert_eq!(rpc1(&w, "result", json!({"id": second}))["status"], "pending");
}

#[test]
fn once_takes_a_no_and_exits() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "no thanks", &public);
    let witness = w.watched_owner();
    let mut approver = w.approver_once_with(&witness.path);
    approver.answer("n");
    let status = approver.exit();
    assert!(status.success(), "{status}: {}", approver.text());
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "denied");
    assert_eq!(witness.reads(), 0);
}

/// `--once` has the same manners as the long-running approver, and tells its
/// caller the request was not answered.
#[test]
fn once_refuses_a_swap_and_exits_nonzero() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("a").status.success());
    assert!(w.encrypt_value("b", b"the other secret\n").status.success());
    fs::copy(w.secret_file("b"), w.secret_file("a")).unwrap();
    let (_, public) = new_key();
    request_raw(&w, "a", "swap", &public);
    let witness = w.watched_owner();
    let mut approver = w.approver_once_with(&witness.path);
    approver.answer("y");
    let status = approver.exit();
    assert!(!status.success(), "{}", approver.text());
    assert_eq!(witness.reads(), 0);
}

#[test]
fn once_shows_a_hostile_purpose_as_text() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let purpose = "safe\x1b[2J\x1b]0;spoof\x07\nsecret: other\u{202e}";
    request_raw(&w, "s", purpose, &public);
    let mut approver = w.approver_once();
    approver.answer("n");
    let status = approver.exit();
    assert!(status.success(), "{status}: {}", approver.text());
    let shown = approver.text();
    assert!(shown.contains("safe"), "{shown:?}");
    assert!(!shown.contains('\x1b') && !shown.contains('\x07') && !shown.contains('\u{202e}'), "{shown:?}");
    assert!(!shown.contains("\nsecret: other"), "the purpose forged a line: {shown:?}");
}

/// A terminal, keys already waiting in it when the question goes up: they
/// must not answer it. Only a real tty has typeahead to flush.
#[test]
fn once_discards_typeahead_before_reading_the_answer() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let (_, public) = new_key();
    let id = request_raw(&w, "s", "typed ahead", &public);
    // `y` is in the tty's queue a second before the approver exists; after the
    // prompt, and a pause in which a surviving `y` would be acted on, `n`.
    let py = r#"
import os, pty, select, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.execv("/bin/sh", ["sh", "-c", 'sleep 1; exec "$@"', "sh"] + sys.argv[1:])
os.write(fd, b"y\n")
buf, deadline, answered = b"", time.time() + 15, False
while time.time() < deadline:
    if not select.select([fd], [], [], 0.1)[0]:
        continue
    try:
        chunk = os.read(fd, 4096)
    except OSError:
        break
    if not chunk:
        break
    buf += chunk
    if b"Approve? [y/N]" in buf and not answered:
        answered = True
        time.sleep(1.5)
        os.write(fd, b"n\n")
sys.stdout.buffer.write(buf)
sys.exit(0 if answered else 3)
"#;
    let out = w.command("/usr/bin/python3").env_remove("PLEXUS_WS_TRACE").args(["-I", "-c", py, APPROVE, "--once", "--identity"]).arg(&w.owner).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    let shown = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "no prompt: {shown} {}", String::from_utf8_lossy(&out.stderr));
    let events = w.audit_events();
    assert!(!events.iter().any(|e| e == "fulfill"), "a key typed ahead answered the prompt: {events:?}\n{shown}");
    assert_eq!(rpc1(&w, "result", json!({"id": id}))["status"], "denied");
}

// ---- HYPE-22: age in process, keys from our own binary -------------------

/// The other half of the witness: a key read for each yes and at no other
/// time, so a held key cannot be what answers the second request.
#[test]
fn the_identity_is_read_at_each_decrypt_and_not_before() {
    let w = World::new();
    let _server = w.server();
    assert!(w.encrypt("s").status.success());
    let witness = w.watched_owner();
    let mut approver = w.approver_with(&witness.path);
    let mut first = w.run_cmd("s", "first", "true");
    wait_until("a prompt", SHELL_WAIT, || approver.text().contains(PROMPT));
    assert_eq!(witness.reads(), 0, "the key was read before the owner said yes");
    approver.answer("y");
    assert!(first.wait().success());
    assert_eq!(witness.reads(), 1);
    // Past the witness's pause, so a second read is seen as one.
    std::thread::sleep(Duration::from_millis(400));
    let mut second = w.run_cmd("s", "second", "true");
    approver.answer("y");
    assert!(second.wait().success());
    assert_eq!(witness.reads(), 2);
    w.assert_no_age_ran();
}

/// As many recipients as the file names, each able to open what is stored.
#[test]
fn any_number_of_recipients_can_open_a_stored_secret() {
    let w = World::new();
    let _server = w.server();
    let keys: Vec<PathBuf> = (0..4).map(|i| w.root.join(format!("extra{i}.key"))).collect();
    let mut list = fs::read_to_string(&w.recipients).unwrap();
    for key in &keys {
        list.push_str(&format!("# another\n{}\n", keygen(key)));
    }
    fs::write(&w.recipients, list).unwrap();
    assert!(w.encrypt("s").status.success());
    let ciphertext = fs::read(w.secret_file("s")).unwrap();
    for key in keys.iter().chain([&w.owner, &w.recovery]) {
        assert_eq!(open_with_key_file(key, &ciphertext).unwrap(), w.value);
    }
    w.assert_no_age_ran();
}

/// A recipient the build cannot use must not be dropped: that secret would be
/// stored without the key its owner listed. Nothing is stored, nothing recorded.
#[test]
fn a_recipient_this_build_cannot_use_stores_nothing() {
    let w = World::new();
    let _server = w.server();
    let mut list = fs::read_to_string(&w.recipients).unwrap();
    list.push_str("age1se1qgg72x2qfk9wg3wh0qg9u0v7l5dkq4jx69fv80p6wdus4nkr3r3vkwnc\n");
    fs::write(&w.recipients, list).unwrap();
    let out = w.encrypt("s");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 3"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!w.secret_file("s").exists());
    assert!(!w.manifest.exists());
    assert!(rpc_owner(&w, "list", json!({})).unwrap().is_empty());
}

/// The real tools with a real `age1tag1…` line, opened by the recovery key: the
/// recipient needs no enclave. A Linux approver handed an enclave identity says
/// so and seals nothing.
#[cfg(not(target_os = "macos"))]
#[test]
fn an_enclave_recipient_encrypts_here_and_an_enclave_identity_is_refused_here() {
    let w = World::new();
    let _server = w.server();
    // A public key only: nothing here can open what is sealed to it.
    let enclave_line = "age1tag1qt8lw0ual6avlwmwatk888yqnmdamm7xfd0wak53ut6elz5c4swx2yqdj4e";
    let mut list = fs::read_to_string(&w.recipients).unwrap();
    list.push_str(enclave_line);
    list.push('\n');
    fs::write(&w.recipients, list).unwrap();
    assert!(w.encrypt("s").status.success());
    let armored = fs::read(w.secret_file("s")).unwrap();
    assert_eq!(open_with_key_file(&w.recovery, &armored).unwrap(), w.value);

    let identity = w.root.join("owner.identity");
    let made = w.command(APPROVE).arg("enrol").arg("--out").arg(&identity).output().unwrap();
    assert!(!made.status.success());
    assert!(String::from_utf8_lossy(&made.stderr).contains("no Secure Enclave"), "{}", String::from_utf8_lossy(&made.stderr));
    assert!(made.stdout.is_empty(), "printed a recipient for no key");
    assert!(!identity.exists());

    let line = bech32_identity();
    fs::write(&identity, format!("{line}\n")).unwrap();
    let mut approver = w.approver_with(&identity);
    let (_id, public) = new_key();
    let request = request_raw(&w, "s", "no enclave here", &public);
    approver.answer("y");
    wait_until("the approver to refuse", SHELL_WAIT, || approver.text().contains("denied"));
    assert!(approver.text().contains("not sealed"), "{}", approver.text());
    assert_eq!(rpc1(&w, "result", json!({"id": request}))["status"], "denied");
}

/// A well-formed line of the enclave's format, with no gate and no key behind it.
#[cfg(not(target_os = "macos"))]
fn bech32_identity() -> String {
    age_core::primitives::bech32_encode(bech32::Hrp::parse_unchecked("age-vault-enclave-"), &[4, 1, 0]).to_uppercase()
}

/// The real enclave, the real tools, and a real finger: only a person can run this.
/// The server is this test's and the WorkOS it signs in to is the fake one, so no
/// deployed vault and no login are needed, and the key dies with the temporary
/// directory. See ENROL.md, step 1.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "fires Touch ID: run by hand, see ENROL.md"]
fn a_real_enclave_round_trip() {
    let w = World::new();
    let _server = w.server();
    let identity = w.root.join("owner.identity");
    let made = w.command(APPROVE).arg("enrol").arg("--out").arg(&identity).output().unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let recipient = String::from_utf8(made.stdout).unwrap();
    let checked = w.command(APPROVE).arg("enrol-check").arg("--identity").arg(&identity).output().unwrap();
    assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stderr));
    assert_eq!(String::from_utf8(checked.stdout).unwrap(), recipient);

    fs::write(&w.recipients, format!("{recipient}{}\n", keygen_line(&w.recovery))).unwrap();
    assert!(w.encrypt("s").status.success());
    let mut approver = w.approver_once_with(&identity);
    let (one_time, public) = new_key();
    let request = request_raw(&w, "s", "a real enclave round trip", &public);
    eprintln!("Touch ID: macOS will ask to \"open a secret in the vault\". Use your finger.");
    approver.answer("y");
    wait_until("the approver to finish", Duration::from_secs(150), || approver.child.try_wait().unwrap().is_some());
    assert!(approver.text().contains("approved and sealed"), "{}", approver.text());
    let answer = rpc1(&w, "result", json!({"id": request}));
    assert_eq!(open(&one_time, answer["sealed"].as_str().unwrap()).unwrap(), w.value);
    w.assert_no_age_ran();
    eprintln!("PASS: encrypted by vault-encrypt, opened through Touch ID by vault-approve");
}

#[test]
fn recovery_keygen_prints_nothing_secret() {
    let w = World::new();
    let key = w.root.join("new-recovery.key");
    // With the approver's environment set: its requirements do not apply here.
    let out = w.command(APPROVE).arg("recovery-keygen").arg("--out").arg(&key).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let printed = stdout.trim();
    assert!(printed.starts_with("age1") && printed.parse::<age::x25519::Recipient>().is_ok(), "{printed}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    let text = fs::read_to_string(&key).unwrap();
    let secret = text.lines().find(|l| l.starts_with("AGE-SECRET-KEY-1")).unwrap();
    for output in [&out.stdout, &out.stderr] {
        assert!(!contains(output, b"AGE-SECRET-KEY"), "{}", String::from_utf8_lossy(output));
        assert!(!contains(output, secret.as_bytes()));
    }
    // What it wrote opens what its printed recipient sealed.
    assert_eq!(open_with_key_file(&key, seal_for(printed, b"it fits").as_bytes()).unwrap(), b"it fits");
}

#[test]
fn recovery_keygen_never_overwrites_a_key() {
    let w = World::new();
    let before = fs::read(&w.recovery).unwrap();
    let out = w.command(APPROVE).arg("recovery-keygen").arg("--out").arg(&w.recovery).output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "printed a recipient for no key");
    assert_eq!(fs::read(&w.recovery).unwrap(), before);
}

// ---- Memory: what the owner's tools keep after they are done. ----

/// Tells a binary built with `memory-scan-test` to stay dumpable, so these tests
/// can read its memory. Any other build has no such switch.
#[cfg(feature = "memory-scan-test")]
const KEEP_DUMPABLE: &str = "VAULT_TEST_KEEP_DUMPABLE";

/// How long a released value was seen to outlive its release (HYPE-22).
#[cfg(feature = "memory-scan-test")]
const AFTER_RELEASE: Duration = Duration::from_millis(300);
#[cfg(feature = "memory-scan-test")]
const RELEASES: usize = 3;

/// One value in every age chunk, so a chunk left anywhere is a hit.
#[cfg(feature = "memory-scan-test")]
fn across_chunks(canary: &str) -> Vec<u8> {
    let mut value = Vec::new();
    while value.len() < 300_000 {
        value.extend_from_slice(canary.as_bytes());
        value.resize(value.len() + 20_000, b'.');
    }
    value
}

/// A port that takes the connection and never speaks: `vault-encrypt` stops
/// there, its encrypting done, for as long as a test wants to look at it.
fn silent_port() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    (listener, url)
}

fn set_memlock_limit_to_zero(command: &mut Command) {
    set_memlock_limit(command, 0);
}

fn set_memlock_limit(command: &mut Command, bytes: u64) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setrlimit is async-signal-safe and `limit` is on this stack.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit { rlim_cur: bytes as libc::rlim_t, rlim_max: bytes as libc::rlim_t };
            if libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// `root` is not bound by RLIMIT_MEMLOCK, so the limit proves nothing there.
fn limits_bind_this_user() -> bool {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() != 0 }
}

fn proc_field(pid: u32, file: &str, field: &str) -> String {
    let text = fs::read_to_string(format!("/proc/{pid}/{file}")).unwrap();
    text.lines().find(|l| l.starts_with(field)).unwrap_or_else(|| panic!("no {field} in {file}: {text}")).to_string()
}

fn assert_cannot_be_dumped(pid: u32) {
    let core = proc_field(pid, "limits", "Max core file size");
    assert_eq!(core.split_whitespace().nth(4), Some("0"), "{core}");
    let denied = fs::File::open(format!("/proc/{pid}/mem")).unwrap_err();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied, "the process is dumpable: {denied}");
}

fn assert_memory_is_locked(pid: u32) {
    let line = proc_field(pid, "status", "VmLck");
    let locked: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(locked > 0, "nothing is locked: {line}");
}

/// Where a freed block lands is the allocator's business, and one look at one
/// release misses it about one time in five on this code. So: a look after each
/// of several releases, in one process, any hit failing.
#[cfg(feature = "memory-scan-test")]
fn a_released_value_leaves_the_approvers_memory(w: &World, value: &[u8]) {
    let _server = w.server();
    assert!(w.encrypt_value("s", value).status.success());
    let mut command = w.command(APPROVE);
    command.env_remove("PLEXUS_WS_TRACE").env(KEEP_DUMPABLE, "1");
    let mut approver = w.approver_from(command, &w.owner);
    let key = fs::read_to_string(&w.owner).unwrap();
    let key = key.lines().find(|l| l.starts_with("AGE-SECRET-KEY-1")).unwrap().to_string();
    for round in 1..=RELEASES {
        // vault-get: a long value is past what an environment variable can hold.
        let caller = w.command(GET).args(["s", "--purpose", "the scan", "--poll-ms", "100"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        approver.answer("y");
        assert_eq!(caller.wait_with_output().unwrap().stdout, value, "release {round} itself");
        wait_until("the approver to report", SHELL_WAIT, || approver.text().matches("approved and sealed").count() == round);
        std::thread::sleep(AFTER_RELEASE);

        let pid = approver.child.id();
        // Control: the scan reads this heap. The credential is held for as long
        // as the approver runs.
        assert!(memory_contains(pid, w.owner_credential.as_bytes()), "control: the scan must see the live heap");
        assert!(!memory_contains(pid, w.canary.as_bytes()), "the value outlived release {round}");
        assert!(!memory_contains(pid, key.as_bytes()), "the identity outlived release {round}");
    }
}

#[test]
#[cfg(feature = "memory-scan-test")]
fn a_short_value_is_gone_from_the_approver_after_its_release() {
    let w = World::new();
    a_released_value_leaves_the_approvers_memory(&w, &w.value);
}

#[test]
#[cfg(feature = "memory-scan-test")]
fn every_chunk_of_a_long_value_is_gone_from_the_approver_after_its_release() {
    let w = World::new();
    a_released_value_leaves_the_approvers_memory(&w, &across_chunks(&w.canary));
}

#[cfg(feature = "memory-scan-test")]
fn the_encrypter_forgets_what_it_encrypted(w: &World, value: &[u8]) {
    let (listener, url) = silent_port();
    let name = format!("scan-{}", &sha_hex(w.canary.as_bytes())[..16]);
    let mut child = w
        .command(ENCRYPT)
        .env("VAULT_URL", url)
        .env(KEEP_DUMPABLE, "1")
        .arg(&name)
        .arg(&w.recipients)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(value).unwrap();
    drop(stdin);
    // It connects only once the ciphertext is finished.
    let (_held_open, _) = listener.accept().unwrap();
    std::thread::sleep(AFTER_RELEASE);
    let found = memory_contains(child.id(), w.canary.as_bytes());
    let control = memory_contains(child.id(), name.as_bytes());
    let _ = child.kill();
    let _ = child.wait();
    assert!(control, "control: the scan must see the live process");
    assert!(!found, "the plaintext outlived the encryption");
}

#[test]
#[cfg(feature = "memory-scan-test")]
fn a_short_value_is_gone_from_the_encrypter_once_encrypted() {
    let w = World::new();
    the_encrypter_forgets_what_it_encrypted(&w, &w.value);
}

#[test]
#[cfg(feature = "memory-scan-test")]
fn every_chunk_of_a_long_value_is_gone_from_the_encrypter_once_encrypted() {
    let w = World::new();
    the_encrypter_forgets_what_it_encrypted(&w, &across_chunks(&w.canary));
}

/// No hook set: what a real run gets. A core piped to a handler ignores
/// RLIMIT_CORE, so the limit alone is not enough, and being undumpable also
/// shuts `/proc/<pid>/mem` to this user's other processes.
#[test]
fn the_owners_tools_cannot_be_dumped_and_lock_their_memory() {
    let w = World::new();
    let _server = w.server();
    let mut approver = w.approver();
    wait_until("the approver to watch", SHELL_WAIT, || approver.text().contains("watching"));
    assert_cannot_be_dumped(approver.child.id());
    assert_memory_is_locked(approver.child.id());
    approver.child.kill().unwrap();

    let (listener, url) = silent_port();
    let mut child = w.command(ENCRYPT).env("VAULT_URL", url).arg("s").arg(&w.recipients).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&w.value).unwrap();
    let (_held_open, _) = listener.accept().unwrap();
    assert_cannot_be_dumped(child.id());
    assert_memory_is_locked(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// The owner's tools at their work: no flag, and no value handled unlocked.
#[test]
fn memory_that_cannot_be_locked_stops_the_owners_tools_by_default() {
    if !limits_bind_this_user() {
        return;
    }
    let w = World::new();
    let _server = w.server();

    let mut encrypter = w.command(ENCRYPT);
    set_memlock_limit_to_zero(&mut encrypter);
    let mut child = encrypter.arg("s").arg(&w.recipients).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // It may exit without reading this.
    let _ = child.stdin.take().unwrap().write_all(&w.value);
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "vault-encrypt went on");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not locked"), "{}", String::from_utf8_lossy(&out.stderr));
    // The warning is the later check speaking: this stopped at the start.
    assert!(!String::from_utf8_lossy(&out.stderr).contains("NOT locked"), "it read the value first: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!w.secret_file("s").exists());
    assert!(!w.manifest.exists());

    let mut approver = w.command(APPROVE);
    set_memlock_limit_to_zero(&mut approver);
    let mut child = approver.arg("--identity").arg(&w.owner).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // Bounded: an approver that went on would watch for ever.
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > SHELL_WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("vault-approve went on");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "vault-approve went on");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not locked"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!contains(&out.stdout, b"watching"), "it connected: {}", String::from_utf8_lossy(&out.stdout));
}

/// The startup probe passes and a large value does not fit: the approver had a
/// yes, and still sends no seal, with no flag.
#[test]
fn a_block_that_fails_to_lock_later_sends_no_seal() {
    if !limits_bind_this_user() {
        return;
    }
    let w = World::new();
    let _server = w.server();
    let large = vec![b'x'; 600 * 1024];
    assert!(w.encrypt_value("s", &large).status.success());

    let mut approver = w.command(APPROVE);
    set_memlock_limit(&mut approver, 512 * 1024);
    approver.env_remove("PLEXUS_WS_TRACE");
    let mut approver = w.approver_from(approver, &w.owner);
    let (_id, public) = new_key();
    let request = request_raw(&w, "s", "too large to lock", &public);
    approver.answer("y");
    // "denied" and not "not sealed": the deny is sent between the two.
    wait_until("the approver to refuse", SHELL_WAIT, || approver.text().contains("denied"));
    assert!(approver.text().contains("not sealed"), "{}", approver.text());
    assert!(approver.text().contains("not locked"), "{}", approver.text());
    assert!(!approver.text().contains("approved and sealed"), "{}", approver.text());
    let answer = rpc1(&w, "result", json!({"id": request}));
    assert_eq!(answer["status"], "denied", "{answer}");
}

/// A block that did not lock is that request's failure, and only that request's.
/// The large value is refused for good reason; the approver then opens a small
/// one with no restart, and the limit never moved.
#[test]
fn a_failed_lock_refuses_that_request_and_not_the_next() {
    if !limits_bind_this_user() {
        return;
    }
    let w = World::new();
    let _server = w.server();
    // As large as the server stores, once armored.
    let large = vec![b'x'; 700 * 1024];
    assert!(w.encrypt_value("large", &large).status.success());
    assert!(w.encrypt_value("small", b"a small value\n").status.success());

    let mut approver = w.command(APPROVE);
    // Room for an ordinary request, and not for the large one.
    set_memlock_limit(&mut approver, 768 * 1024);
    approver.env_remove("PLEXUS_WS_TRACE");
    let mut approver = w.approver_from(approver, &w.owner);

    let (_id, public) = new_key();
    let refused = request_raw(&w, "large", "too large to lock", &public);
    approver.answer("y");
    wait_until("the approver to refuse", SHELL_WAIT, || approver.text().contains("denied"));
    assert!(approver.text().contains("not sealed"), "{}", approver.text());
    assert_eq!(rpc1(&w, "result", json!({"id": refused}))["status"], "denied");

    let (one_time, public) = new_key();
    let request = request_raw(&w, "small", "after the failure", &public);
    wait_until("the next request to show", SHELL_WAIT, || approver.text().contains("after the failure"));
    approver.answer("y");
    wait_until("the approver to answer", SHELL_WAIT, || approver.text().matches("denied").count() > 1 || approver.text().contains("approved and sealed"));
    assert!(approver.text().contains("approved and sealed"), "one failure outlived its request: {}", approver.text());
    let answer = rpc1(&w, "result", json!({"id": request}));
    assert_eq!(open(&one_time, answer["sealed"].as_str().unwrap()).unwrap(), b"a small value\n");
}

/// Recovery on a bad day: it warns, and the value still comes out.
#[test]
fn recovery_warns_about_memory_it_cannot_lock_and_goes_on() {
    if !limits_bind_this_user() {
        return;
    }
    let w = World::new();
    {
        let _server = w.server();
        assert!(w.encrypt("s").status.success());
    }
    let o = w.offline("s");
    let mut recover = w.recover("s", &o);
    set_memlock_limit_to_zero(&mut recover);
    let out = recover.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, w.value);
    assert!(String::from_utf8_lossy(&out.stderr).contains("NOT locked"), "{}", String::from_utf8_lossy(&out.stderr));

    let mut strict = w.recover("s", &o);
    set_memlock_limit_to_zero(&mut strict);
    refused_with(&strict.arg("--require-memory-lock").output().unwrap(), "not locked");
}

#[test]
fn require_memory_lock_stops_before_any_value_is_touched() {
    if !limits_bind_this_user() {
        return;
    }
    let w = World::new();
    let _server = w.server();

    let mut encrypter = w.command(ENCRYPT);
    set_memlock_limit_to_zero(&mut encrypter);
    let mut child = encrypter.arg("s").arg(&w.recipients).arg("--require-memory-lock").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // It may exit without reading this.
    let _ = child.stdin.take().unwrap().write_all(&w.value);
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not locked"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!w.secret_file("s").exists());
    assert!(!w.manifest.exists());

    assert!(w.encrypt("t").status.success());
    let mut approver = w.command(APPROVE);
    set_memlock_limit_to_zero(&mut approver);
    let out = approver.arg("--identity").arg(&w.owner).arg("--require-memory-lock").stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not locked"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!contains(&out.stdout, b"watching"), "it connected: {}", String::from_utf8_lossy(&out.stdout));
}

// ---- Recovery: the Mac is gone; the offline key, a copy of the manifest and a
// ---- copy of the ciphertext are all there is.

struct Offline {
    dir: PathBuf,
    ciphertext: PathBuf,
    key: PathBuf,
    manifest: PathBuf,
}

impl World {
    /// What the owner carries to another machine for `name`.
    fn offline(&self, name: &str) -> Offline {
        let dir = self.root.join("offline");
        fs::create_dir_all(&dir).unwrap();
        let (ciphertext, key, manifest) = (dir.join(format!("{name}.age")), dir.join("recovery.key"), dir.join("manifest.json"));
        fs::copy(self.secret_file(name), &ciphertext).unwrap();
        fs::copy(&self.recovery, &key).unwrap();
        fs::copy(&self.manifest, &manifest).unwrap();
        Offline { dir, ciphertext, key, manifest }
    }

    fn recover(&self, name: &str, o: &Offline) -> Command {
        let mut command = self.command(APPROVE);
        command.env_remove("VAULT_MANIFEST").arg("recover").arg(name).arg("--ciphertext").arg(&o.ciphertext).arg("--identity").arg(&o.key).arg("--manifest").arg(&o.manifest);
        command
    }
}

fn refused_with(out: &std::process::Output, word: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "it succeeded: {stderr}");
    assert!(out.stdout.is_empty(), "something reached stdout");
    assert!(stderr.contains(word), "{stderr}");
}

/// The whole of it through our own binaries, with an empty PATH.
#[test]
fn a_full_recovery_needs_no_age_tool() {
    let w = World::new();
    let empty = w.root.join("empty-path");
    fs::create_dir_all(&empty).unwrap();
    let no_tool = |mut c: Command| {
        c.env_clear().env("HOME", &w.home).env("PATH", &empty);
        c
    };
    // Control: nothing on this PATH.
    let mut control = no_tool(Command::new("/bin/sh"));
    assert!(!control.args(["-c", "command -v age || command -v age-keygen || command -v rage"]).status().unwrap().success());

    // Enrolled the way ENROL.md says, the recovery key by our own keygen.
    let key = w.root.join("made-for-recovery.key");
    let mut keygen = no_tool(Command::new(APPROVE));
    let made = keygen.arg("recovery-keygen").arg("--out").arg(&key).output().unwrap();
    assert!(made.status.success());
    let recipient = String::from_utf8(made.stdout).unwrap();
    fs::write(&w.recipients, format!("{}\n{recipient}", keygen_line(&w.owner))).unwrap();
    {
        let _server = w.server();
        assert!(w.encrypt("s").status.success());
    }
    // The server and the Mac are gone.
    fs::remove_file(&w.owner).unwrap();
    let mut o = w.offline("s");
    o.key = key;
    let before: Vec<_> = fs::read_dir(&o.dir).unwrap().map(|e| e.unwrap().path()).collect();

    let mut recover = no_tool(Command::new(APPROVE));
    let out = recover.arg("recover").arg("s").arg("--ciphertext").arg(&o.ciphertext).arg("--identity").arg(&o.key).arg("--manifest").arg(&o.manifest).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, w.value);
    assert!(!contains(&out.stderr, w.canary.as_bytes()));

    let after: Vec<_> = fs::read_dir(&o.dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(after, before, "recovery wrote a file");
    w.assert_no_age_ran();
    assert_eq!(w.leaks(&[&w.state, &w.logs, &w.home, &o.dir]), Vec::<String>::new());
}

/// A recipient for a key that exists only in a test's public half.
fn keygen_line(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    text.lines().find_map(|l| l.strip_prefix("# public key: ")).unwrap().to_string()
}

#[test]
fn recovery_offers_no_way_to_write_a_file() {
    let w = World::new();
    let help = w.command(APPROVE).args(["recover", "--help"]).output().unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(help.status.success() && text.contains("--ciphertext"), "{text}");
    for flag in ["--out", "--output", "-o "] {
        assert!(!text.contains(flag), "{text}");
    }
}

#[test]
fn recovery_refuses_a_swapped_ciphertext_before_the_key_is_read() {
    let w = World::new();
    {
        let _server = w.server();
        assert!(w.encrypt("a").status.success());
        assert!(w.encrypt_value("b", b"the other secret\n").status.success());
    }
    let o = w.offline("a");
    fs::copy(w.secret_file("b"), &o.ciphertext).unwrap();
    // Control: it opens, so only the hash can stop it.
    assert_eq!(open_with_key_file(&o.key, &fs::read(&o.ciphertext).unwrap()).unwrap(), b"the other secret\n");
    let witness = KeyWitness::new(&w.root.join("recovery-fifo.key"), fs::read(&w.recovery).unwrap());
    let out = w.recover("a", &Offline { key: witness.path.clone(), ..o }).output().unwrap();
    refused_with(&out, "does not match");
    assert_eq!(witness.reads(), 0, "the key was read for a ciphertext the manifest does not know");
}

#[test]
fn recovery_refuses_a_name_or_a_manifest_it_cannot_vouch_for() {
    let w = World::new();
    {
        let _server = w.server();
        assert!(w.encrypt("s").status.success());
    }
    let o = w.offline("s");
    refused_with(&w.recover("not-enrolled", &o).output().unwrap(), "no entry");
    fs::remove_file(&o.manifest).unwrap();
    refused_with(&w.recover("s", &o).output().unwrap(), "reading the manifest");
}

#[test]
fn recovery_refuses_a_wrong_key_and_prints_no_part_of_a_truncated_value() {
    use age::armor::{ArmoredReader, ArmoredWriter, Format};
    let w = World::new();
    let value = vec![b'v'; 200_000];
    {
        let _server = w.server();
        assert!(w.encrypt_value("s", &value).status.success());
    }
    let mut o = w.offline("s");
    let wrong = w.root.join("wrong.key");
    keygen(&wrong);
    let right = std::mem::replace(&mut o.key, wrong);
    refused_with(&w.recover("s", &o).output().unwrap(), "does not open");
    o.key = right;

    // Cut mid-payload, the header good, and the hash of what was cut on record:
    // only the last chunk's tag can tell.
    let armored = fs::read(&o.ciphertext).unwrap();
    let mut binary = Vec::new();
    ArmoredReader::new(&armored[..]).read_to_end(&mut binary).unwrap();
    binary.truncate(binary.len() - 100_000);
    let mut writer = ArmoredWriter::wrap_output(Vec::new(), Format::AsciiArmor).unwrap();
    writer.write_all(&binary).unwrap();
    let cut = String::from_utf8(writer.finish().unwrap()).unwrap();
    fs::write(&o.ciphertext, &cut).unwrap();
    hyperforge_vault::manifest::record(&o.manifest, "s", &cut).unwrap();
    refused_with(&w.recover("s", &o).output().unwrap(), "reading");
}

#[test]
fn recovery_refuses_a_terminal_unless_told() {
    let w = World::new();
    {
        let _server = w.server();
        assert!(w.encrypt("s").status.success());
    }
    let o = w.offline("s");
    let py = "import pty,sys; sys.exit(pty.spawn(sys.argv[1:]) >> 8)";
    let args = |extra: &[&str]| {
        let mut command = w.command("/usr/bin/python3");
        command.args(["-I", "-c", py, APPROVE, "recover", "s"]).arg("--ciphertext").arg(&o.ciphertext).arg("--identity").arg(&o.key).arg("--manifest").arg(&o.manifest).args(extra);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    };
    let refused = args(&[]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("terminal"), "{}", String::from_utf8_lossy(&refused.stdout));
    assert!(!contains(&refused.stdout, w.canary.as_bytes()));

    let allowed = args(&["--allow-terminal"]).output().unwrap();
    assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stdout));
    assert!(contains(&allowed.stdout, w.canary.as_bytes()));
}
