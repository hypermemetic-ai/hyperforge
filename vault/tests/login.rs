//! `vault-login` against the fake WorkOS: the test plays the owner approving
//! the code. Nothing here reaches WorkOS.

#[path = "support/idp.rs"]
mod idp;

use idp::{nonce, Idp, Reply, CLIENT, OTHER_CLIENT, OTHER_USER, OWNER};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LOGIN: &str = env!("CARGO_BIN_EXE_vault-login");
const ENCRYPT: &str = env!("CARGO_BIN_EXE_vault-encrypt");
const APPROVE: &str = env!("CARGO_BIN_EXE_vault-approve");
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const GOOD_URI: &str = "https://acme.authkit.app/device";
const USER_CODE: &str = "RRGQ-BJVS";
/// Long enough for every flow here; a tool still running then has failed.
const LIMIT: Duration = Duration::from_secs(25);

struct Bench {
    idp: Idp,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    file: PathBuf,
    device_code: String,
}

struct Ran {
    /// None if it had to be killed.
    success: Option<bool>,
    took: Duration,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn succeeded(&self) -> bool {
        self.success == Some(true)
    }

    fn failed(&self) -> bool {
        self.success == Some(false)
    }

    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

impl Bench {
    fn new() -> Bench {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        // The browser is a script that writes down what it was asked to open.
        for name in ["open", "xdg-open"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$VAULT_TEST_OPENED\"\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let file = root.join("login.json");
        Bench { idp: Idp::start(), _tmp: tmp, root, file, device_code: format!("device-{}", nonce()) }
    }

    /// The docs' answer, at the shortest interval, so tests are quick.
    fn authorize(&self) -> Value {
        json!({
            "device_code": self.device_code,
            "user_code": USER_CODE,
            "verification_uri": GOOD_URI,
            "verification_uri_complete": format!("{GOOD_URI}?user_code={USER_CODE}"),
            "expires_in": 20,
            "interval": 1,
        })
    }

    fn opened(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("opened")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// The opener is detached from the tool, so it may finish just after.
    fn opened_after_a_moment(&self) -> Vec<String> {
        std::thread::sleep(Duration::from_millis(300));
        self.opened()
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("VAULT_TEST_OPENED", self.root.join("opened"))
            .env("VAULT_WORKOS_CLIENT_ID", CLIENT)
            .env("VAULT_WORKOS_BASE", &self.idp.base)
            .env("VAULT_LOGIN_FILE", &self.file)
            .env("RUST_LOG", "trace")
            .env("RUST_BACKTRACE", "full");
        command
    }

    fn run(&self, mut command: Command, stdin: Stdio) -> Ran {
        let n = nonce();
        let (out, err) = (self.root.join(format!("{n}.stdout")), self.root.join(format!("{n}.stderr")));
        command.stdin(stdin).stdout(fs::File::create(&out).unwrap()).stderr(fs::File::create(&err).unwrap());
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        let success = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status.success());
            }
            if started.elapsed() > LIMIT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Ran { success, took: started.elapsed(), stdout: fs::read_to_string(out).unwrap(), stderr: fs::read_to_string(err).unwrap() }
    }

    fn login(&self, args: &[&str]) -> Ran {
        let mut command = self.command(LOGIN);
        command.env("VAULT_OWNER_USER_ID", OWNER).args(args);
        self.run(command, Stdio::null())
    }

    fn tokens(&self, user: &str) -> Reply {
        Reply::Tokens { access: self.idp.token(user), refresh: format!("refresh-{}", nonce()) }
    }

    fn assert_stored_nothing(&self) {
        assert!(!self.file.exists(), "a login file exists");
        assert!(!self.file.with_extension("tmp").exists(), "a temporary file exists");
    }

    /// Every file under the bench but the login itself, the tools' stdout and
    /// stderr among them, for each secret and its encodings.
    fn assert_no_leak(&self, secrets: &[&str]) {
        let mut hits = Vec::new();
        for secret in secrets {
            assert!(secret.len() >= 8, "not a needle: {secret}");
            scan(&self.root, &self.file, secret, &mut hits);
        }
        assert_eq!(hits, Vec::<String>::new());
    }
}

fn needles(secret: &str) -> Vec<Vec<u8>> {
    use base64::Engine;
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    vec![
        secret.as_bytes().to_vec(),
        base64::engine::general_purpose::STANDARD.encode(secret).into_bytes(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret).into_bytes(),
        hex.into_bytes(),
    ]
}

fn scan(path: &Path, skip: &Path, secret: &str, hits: &mut Vec<String>) {
    if path == skip {
        return;
    }
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            scan(&entry.unwrap().path(), skip, secret, hits);
        }
        return;
    }
    let bytes = fs::read(path).unwrap();
    for needle in needles(secret) {
        if bytes.windows(needle.len()).any(|w| w == needle.as_slice()) {
            hits.push(path.display().to_string());
        }
    }
}

fn gaps(seen: &[idp::Seen]) -> Vec<Duration> {
    seen.windows(2).map(|pair| pair[1].at - pair[0].at).collect()
}

fn stored(file: &Path) -> Value {
    serde_json::from_slice(&fs::read(file).unwrap()).unwrap()
}

// ---- The flow ---------------------------------------------------------------

#[test]
fn the_device_flow_signs_in_with_no_secret_and_stores_owner_only() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    let access = b.idp.token(OWNER);
    let refresh = format!("refresh-{}", nonce());
    b.idp.polls(vec![Reply::Error("authorization_pending"), Reply::Error("authorization_pending"), Reply::Tokens { access: access.clone(), refresh: refresh.clone() }]);

    let ran = b.login(&["--no-browser"]);
    assert!(ran.succeeded(), "{}", ran.all());

    assert!(ran.stdout.contains(GOOD_URI), "{}", ran.stdout);
    assert!(ran.stdout.contains(USER_CODE), "{}", ran.stdout);
    assert!(ran.stdout.to_lowercase().contains("you started"), "no warning: {}", ran.stdout);

    let (asked, polls) = (b.idp.seen_authorize(), b.idp.seen_polls());
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].form.len(), 1, "only the client id: {:?}", asked[0].form.keys());
    assert_eq!(asked[0].form["client_id"], CLIENT);
    assert_eq!(polls.len(), 3);
    for poll in &polls {
        assert!(poll.content_type.starts_with("application/x-www-form-urlencoded"));
        assert_eq!(poll.form["grant_type"], GRANT);
        assert_eq!(poll.form["device_code"], b.device_code);
        assert_eq!(poll.form["client_id"], CLIENT);
        assert_eq!(poll.form.len(), 3, "{:?}", poll.form.keys());
    }

    let tokens = stored(&b.file);
    assert_eq!(tokens["access_token"], access.as_str());
    assert_eq!(tokens["refresh_token"], refresh.as_str());
    assert_eq!(fs::metadata(&b.file).unwrap().permissions().mode() & 0o777, 0o600);
    b.assert_no_leak(&[&access, &refresh, &b.device_code]);
}

/// The scan has to be able to find what it looks for.
#[test]
fn the_leak_scan_finds_what_is_planted() {
    let b = Bench::new();
    let secret = format!("secret-{}", nonce());
    for (i, needle) in needles(&secret).iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("planted"), needle).unwrap();
        let mut hits = Vec::new();
        scan(dir.path(), &b.file, &secret, &mut hits);
        assert!(!hits.is_empty(), "encoding {i}");
    }
}

#[test]
fn pending_waits_the_interval_from_the_start() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    b.idp.polls(vec![Reply::Error("authorization_pending"), Reply::Error("authorization_pending"), b.tokens(OWNER)]);
    let ran = b.login(&["--no-browser"]);
    assert!(ran.succeeded(), "{}", ran.all());
    let polls = b.idp.seen_polls();
    assert!(polls[0].at - b.idp.seen_authorize()[0].at >= Duration::from_millis(950), "the first poll waits too");
    assert_eq!(polls.len(), 3);
    for gap in gaps(&polls) {
        assert!(gap >= Duration::from_millis(950), "{gap:?}");
    }
}

#[test]
fn an_answer_with_no_interval_polls_every_five_seconds() {
    let b = Bench::new();
    let mut authorize = b.authorize();
    authorize.as_object_mut().unwrap().remove("interval");
    b.idp.device(authorize);
    b.idp.polls(vec![b.tokens(OWNER)]);
    assert!(b.login(&["--no-browser"]).succeeded());
    assert!(b.idp.seen_polls()[0].at - b.idp.seen_authorize()[0].at >= Duration::from_millis(4950));
}

#[test]
fn slow_down_raises_the_interval_for_good() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    b.idp.polls(vec![Reply::Error("slow_down"), Reply::Error("authorization_pending"), b.tokens(OWNER)]);
    // RFC 8628 3.5: five more seconds. The docs ask only for more.
    let ran = b.login(&["--no-browser"]);
    assert!(ran.succeeded(), "{}", ran.all());
    let gaps = gaps(&b.idp.seen_polls());
    assert_eq!(gaps.len(), 2);
    assert!(gaps[0] >= Duration::from_millis(5950), "{gaps:?}");
    assert!(gaps[1] >= Duration::from_millis(5950), "it stays raised: {gaps:?}");
}

#[test]
fn denied_and_expired_stop_at_once_and_store_nothing() {
    for code in ["access_denied", "expired_token"] {
        let b = Bench::new();
        b.idp.device(b.authorize());
        b.idp.polls(vec![Reply::Error("authorization_pending"), Reply::Error(code), b.tokens(OWNER)]);
        let ran = b.login(&["--no-browser"]);
        assert!(ran.failed(), "{code}: {}", ran.all());
        assert_eq!(b.idp.seen_polls().len(), 2, "{code}: no poll after it");
        b.assert_stored_nothing();
        b.assert_no_leak(&[&b.device_code]);
    }
}

/// Bounded by the server's `expires_in` even if the server never says stop.
#[test]
fn polling_ends_at_expires_in_with_no_word_from_the_server() {
    let b = Bench::new();
    let mut authorize = b.authorize();
    authorize["expires_in"] = json!(3);
    b.idp.device(authorize);
    b.idp.polls(vec![Reply::Error("authorization_pending")]);
    let ran = b.login(&["--no-browser"]);
    assert!(ran.failed(), "{}", ran.all());
    assert!(ran.took < Duration::from_secs(5), "{:?}", ran.took);
    assert!(!b.idp.seen_polls().is_empty());
    let last = b.idp.seen_polls().last().unwrap().at;
    assert!(last - b.idp.seen_authorize()[0].at <= Duration::from_secs(3));
    b.assert_stored_nothing();
}

#[test]
fn an_interval_past_the_expiry_ends_without_waiting_for_it() {
    let b = Bench::new();
    let mut authorize = b.authorize();
    authorize["expires_in"] = json!(2);
    authorize["interval"] = json!(600);
    b.idp.device(authorize);
    b.idp.polls(vec![b.tokens(OWNER)]);
    let ran = b.login(&["--no-browser"]);
    assert!(ran.failed(), "{}", ran.all());
    assert!(ran.took < Duration::from_secs(4), "{:?}", ran.took);
    assert_eq!(b.idp.seen_polls().len(), 0);
    b.assert_stored_nothing();
}

#[test]
fn anything_unexpected_while_polling_stops_and_echoes_nothing() {
    let echo = format!("echoed-{}", nonce());
    let replies = vec![
        Reply::Raw("400 Bad Request", json!({"error": "something_new", "error_description": echo}).to_string()),
        Reply::Raw("400 Bad Request", json!({"error": echo}).to_string()),
        Reply::Raw("400 Bad Request", echo.clone()),
        Reply::Raw("500 Internal Server Error", echo.clone()),
        Reply::Raw("200 OK", echo.clone()),
        Reply::Raw("200 OK", json!({"access_token": echo}).to_string()),
        Reply::Raw("200 OK", json!({"access_token": 7, "refresh_token": echo}).to_string()),
        Reply::Raw("200 OK", json!({"access_token": echo, "refresh_token": 7}).to_string()),
        Reply::Raw("200 OK", json!({"access_token": echo, "refresh_token": "r"}).to_string()),
        Reply::Raw("302 Found", echo.clone()),
    ];
    for (i, reply) in replies.into_iter().enumerate() {
        let b = Bench::new();
        b.idp.device(b.authorize());
        b.idp.polls(vec![reply]);
        let ran = b.login(&["--no-browser"]);
        assert!(ran.failed(), "case {i}: {}", ran.all());
        assert_eq!(b.idp.seen_polls().len(), 1, "case {i}");
        b.assert_stored_nothing();
        b.assert_no_leak(&[&echo, &b.device_code]);
    }
}

#[test]
fn a_bad_authorization_answer_never_polls() {
    let probe = Bench::new();
    let good = probe.authorize();
    let without = |key: &str| {
        let mut a = good.clone();
        a.as_object_mut().unwrap().remove(key);
        a
    };
    let with = |key: &str, value: Value| {
        let mut a = good.clone();
        a[key] = value;
        a
    };
    let cases = vec![
        without("device_code"),
        without("user_code"),
        without("verification_uri"),
        without("expires_in"),
        with("expires_in", json!(0)),
        with("expires_in", json!(-1)),
        with("expires_in", json!("soon")),
        with("interval", json!(-1)),
        with("device_code", json!("")),
        with("user_code", json!("")),
        with("verification_uri", json!("")),
    ];
    for (i, authorize) in cases.into_iter().enumerate() {
        let b = Bench::new();
        b.idp.device(authorize);
        b.idp.polls(vec![b.tokens(OWNER)]);
        let ran = b.login(&["--no-browser"]);
        assert!(ran.failed(), "case {i}: {}", ran.all());
        assert_eq!(b.idp.seen_polls().len(), 0, "case {i}");
        b.assert_stored_nothing();
    }
    let b = Bench::new();
    let echo = format!("echoed-{}", nonce());
    b.idp.device_raw("500 Internal Server Error", &json!({"error": echo}).to_string());
    let ran = b.login(&["--no-browser"]);
    assert!(ran.failed(), "{}", ran.all());
    assert_eq!(b.idp.seen_polls().len(), 0);
    b.assert_no_leak(&[&echo]);
}

// ---- Whose token this is ------------------------------------------------------

#[test]
fn a_token_for_another_user_is_refused_and_stored_nowhere() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    let access = b.idp.token(OTHER_USER);
    let refresh = format!("refresh-{}", nonce());
    b.idp.polls(vec![Reply::Tokens { access: access.clone(), refresh: refresh.clone() }]);
    fs::write(&b.file, r#"{"access_token":"a","refresh_token":"the owner's"}"#).unwrap();
    let before = fs::read(&b.file).unwrap();
    let ran = b.login(&["--no-browser"]);
    assert!(ran.failed(), "{}", ran.all());
    assert!(ran.all().contains("not the owner"), "{}", ran.all());
    assert!(!ran.all().contains(OTHER_USER), "says nothing of who: {}", ran.all());
    assert_eq!(fs::read(&b.file).unwrap(), before, "the owner's sign-in stands");
    assert!(!b.file.with_extension("tmp").exists());
    fs::remove_file(&b.file).unwrap();
    b.assert_no_leak(&[&access, &refresh, &b.device_code]);
}

#[test]
fn a_token_the_validator_refuses_stores_nothing() {
    let probe = Bench::new();
    let claims = probe.idp.claims(OWNER);
    let changed = |key: &str, value: Value| {
        let mut c = claims.clone();
        c[key] = value;
        c
    };
    let cases: Vec<(&str, Value, bool)> = vec![
        ("another client", changed("client_id", json!(OTHER_CLIENT)), false),
        ("no client", {
            let mut c = claims.clone();
            c.as_object_mut().unwrap().remove("client_id");
            c
        }, false),
        ("expired", changed("exp", json!(idp::now() - 10)), false),
        ("forged", claims.clone(), true),
    ];
    for (name, claims, forge) in cases {
        let b = Bench::new();
        b.idp.device(b.authorize());
        let mut claims = claims;
        claims["iss"] = json!(b.idp.issuer());
        let access = if forge { b.idp.forge(&claims) } else { b.idp.sign(&claims) };
        b.idp.polls(vec![Reply::Tokens { access, refresh: format!("refresh-{}", nonce()) }]);
        let ran = b.login(&["--no-browser"]);
        assert!(ran.failed(), "{name}: {}", ran.all());
        b.assert_stored_nothing();
    }
    let b = Bench::new();
    b.idp.device(b.authorize());
    let mut other_issuer = b.idp.claims(OWNER);
    other_issuer["iss"] = json!("https://api.workos.com/user_management/client_someone_else");
    b.idp.polls(vec![Reply::Tokens { access: b.idp.sign(&other_issuer), refresh: format!("refresh-{}", nonce()) }]);
    let ran = b.login(&["--no-browser"]);
    assert!(ran.failed(), "another issuer: {}", ran.all());
    b.assert_stored_nothing();
}

#[test]
fn with_no_owner_configured_nothing_is_even_asked() {
    for owner in [None, Some("")] {
        let b = Bench::new();
        b.idp.device(b.authorize());
        let mut command = b.command(LOGIN);
        if let Some(owner) = owner {
            command.env("VAULT_OWNER_USER_ID", owner);
        }
        command.arg("--no-browser");
        let ran = b.run(command, Stdio::null());
        assert!(ran.failed(), "{}", ran.all());
        assert_eq!(b.idp.seen_authorize().len(), 0);
    }
}

// ---- The browser --------------------------------------------------------------

#[test]
fn a_workos_host_is_opened_and_the_prefilled_link_is_not_trusted_instead() {
    for uri in ["https://acme.authkit.app/device", "https://authkit.app/device", "https://auth.workos.com/device", "https://workos.com/device", "https://ACME.AuthKit.app/device"] {
        let b = Bench::new();
        let mut authorize = b.authorize();
        authorize["verification_uri"] = json!(uri);
        authorize["verification_uri_complete"] = json!("https://evil.example/device?user_code=X");
        b.idp.device(authorize);
        b.idp.polls(vec![b.tokens(OWNER)]);
        let ran = b.login(&[]);
        assert!(ran.succeeded(), "{uri}: {}", ran.all());
        let opened = b.opened_after_a_moment();
        assert_eq!(opened.len(), 1, "{uri}: {opened:?}");
        assert_eq!(opened[0].to_lowercase(), uri.to_lowercase());
    }
}

#[test]
fn any_other_host_is_printed_and_never_opened() {
    for uri in [
        "https://evil.example/device",
        "https://authkit.app.evil.example/device",
        "https://workos.com.evil.example/device",
        "https://notauthkit.app/device",
        "https://evilworkos.com/device",
        "https://acme.authkit.app@evil.example/device",
        "https://acme.authkit.app:8443/device",
        "https://user:pw@acme.authkit.app/device",
        "http://acme.authkit.app/device",
        "file:///etc/passwd",
        "not a url",
    ] {
        let b = Bench::new();
        let mut authorize = b.authorize();
        authorize["verification_uri"] = json!(uri);
        b.idp.device(authorize);
        b.idp.polls(vec![b.tokens(OWNER)]);
        let ran = b.login(&[]);
        assert!(ran.stdout.contains(uri), "{uri}: printed so the owner can judge it: {}", ran.all());
        assert!(ran.succeeded(), "{uri}: {}", ran.all());
        assert_eq!(b.opened_after_a_moment(), Vec::<String>::new(), "{uri}");
    }
}

#[test]
fn no_browser_opens_nothing_even_on_a_good_host() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    b.idp.polls(vec![b.tokens(OWNER)]);
    assert!(b.login(&["--no-browser"]).succeeded());
    assert_eq!(b.opened_after_a_moment(), Vec::<String>::new());
}

#[test]
fn what_the_server_prints_cannot_steer_the_terminal() {
    let b = Bench::new();
    let mut authorize = b.authorize();
    authorize["user_code"] = json!("ABCD\u{1b}[2J\u{202e}EFGH");
    authorize["verification_uri"] = json!("https://evil.example/\u{1b}]0;x\u{7}device");
    b.idp.device(authorize);
    b.idp.polls(vec![b.tokens(OWNER)]);
    let ran = b.login(&[]);
    assert!(ran.succeeded(), "{}", ran.all());
    assert!(!ran.stdout.contains('\u{1b}') && !ran.stdout.contains('\u{202e}') && !ran.stdout.contains('\u{7}'), "{:?}", ran.stdout);
    assert_eq!(b.opened_after_a_moment(), Vec::<String>::new());
}

// ---- After the sign-in --------------------------------------------------------

/// What the device flow stored is what the refresh path uses: the owner's tool
/// gets a new pair, and the rotated token is on disk before anything else.
#[test]
fn the_stored_sign_in_refreshes_and_rotates() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    let refresh = format!("refresh-{}", nonce());
    b.idp.polls(vec![Reply::Tokens { access: b.idp.token_lasting(OWNER, 3), refresh: refresh.clone() }]);
    assert!(b.login(&["--no-browser"]).succeeded());
    b.idp.accept_refresh(&refresh, 3600);

    let recovery = b.root.join("recovery.key");
    let keygen = b.run({
        let mut c = b.command(APPROVE);
        c.args(["recovery-keygen", "--out"]).arg(&recovery);
        c
    }, Stdio::null());
    assert!(keygen.succeeded(), "{}", keygen.all());
    let recipients = b.root.join("recipients.txt");
    fs::write(&recipients, &keygen.stdout).unwrap();

    // Nothing listens at the vault: the sign-in has happened by then, and the
    // tool stops there.
    let mut encrypt = b.command(ENCRYPT);
    encrypt.env("VAULT_URL", "ws://127.0.0.1:9").env("VAULT_MANIFEST", b.root.join("manifest.json")).arg("s").arg(&recipients);
    let value = fs::File::open(&recovery).unwrap();
    let ran = b.run(encrypt, Stdio::from(value));
    assert!(ran.failed(), "{}", ran.all());

    assert_eq!(b.idp.refreshes(), 1, "{}", ran.all());
    let tokens = stored(&b.file);
    assert_ne!(tokens["refresh_token"], refresh.as_str(), "the rotated token is on disk");
    assert!(tokens["refresh_token"].as_str().unwrap().starts_with("refresh-"));
    assert!(idp_expiry(tokens["access_token"].as_str().unwrap()) > idp::now() + 3000);
    fs::remove_file(&recovery).unwrap();
    b.assert_no_leak(&[tokens["access_token"].as_str().unwrap(), tokens["refresh_token"].as_str().unwrap(), &refresh, &b.device_code]);
}

fn idp_expiry(token: &str) -> i64 {
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap();
    serde_json::from_slice::<Value>(&payload).unwrap()["exp"].as_i64().unwrap()
}

// ---- What is gone -------------------------------------------------------------

#[test]
fn no_trace_of_the_loopback_flow_is_left() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in ["src/login.rs", "src/bin/vault-login.rs", "src/bin/shared/workos.rs"] {
        let text = fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for word in ["TcpListener", "redirect_uri", "--redirect-uri", "VAULT_REDIRECT_URI", "code_verifier", "code_challenge", "authorization_code", "127.0.0.1", "provider"] {
            assert!(!text.contains(word), "{file} says {word}");
        }
    }
}

#[test]
fn the_redirect_flags_are_refused() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    for flag in ["--redirect-uri", "--provider"] {
        let ran = b.login(&[flag, "x", "--no-browser"]);
        assert!(ran.failed(), "{flag}: {}", ran.all());
    }
    assert_eq!(b.idp.seen_authorize().len(), 0);
}

/// The refresh token is on this process's heap: no core of it, and no reading it
/// through `/proc` by this user's other processes. The tool is held mid-sign-in,
/// polling a code nobody approves: once it has asked for that code, it is past
/// `undumpable`.
#[cfg(target_os = "linux")]
#[test]
fn the_login_tool_cannot_be_dumped() {
    let b = Bench::new();
    b.idp.device(b.authorize());
    b.idp.polls(vec![Reply::Error("authorization_pending")]);
    let mut command = b.command(LOGIN);
    command.env("VAULT_OWNER_USER_ID", OWNER).arg("--no-browser").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    while b.idp.seen_authorize().is_empty() {
        assert!(started.elapsed() < LIMIT, "it never asked for a code");
        assert!(child.try_wait().unwrap().is_none(), "it stopped before asking for a code");
        std::thread::sleep(Duration::from_millis(20));
    }
    let limits = fs::read_to_string(format!("/proc/{}/limits", child.id())).unwrap();
    let core = limits.lines().find(|l| l.starts_with("Max core file size")).unwrap();
    assert_eq!(core.split_whitespace().nth(4), Some("0"), "{core}");
    let denied = fs::File::open(format!("/proc/{}/mem", child.id())).unwrap_err();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied, "dumpable: {denied}");
    child.kill().unwrap();
    child.wait().unwrap();
}
