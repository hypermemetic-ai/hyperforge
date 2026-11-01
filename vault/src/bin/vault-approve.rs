//! The owner's side, on the Mac. It watches the vault, shows each request, and
//! on a yes connects `age -d` straight to `age -r`: the plaintext crosses one
//! OS pipe between two age processes and never enters this one.

use anyhow::{bail, Context};
use clap::Parser;
use futures::StreamExt;
use plexus_client::ClientEvent;
use hyperforge_vault::client::{fingerprint, Rpc};
use hyperforge_vault::login::{Login, WorkosArgs};
use hyperforge_vault::manifest;
use hyperforge_vault::wire::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

/// How long Touch ID may be left waiting before the attempt is abandoned.
const DECRYPT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Parser, Debug)]
#[command(name = "vault-approve", about = "Approve or deny vault requests; a yes decrypts through age, so Touch ID fires")]
struct Args {
    /// The age identity: on the Mac, the age-plugin-se identity file.
    #[arg(long, env = "VAULT_IDENTITY")]
    identity: PathBuf,
    #[arg(long, env = "VAULT_URL", default_value = "ws://127.0.0.1:4470")]
    server: String,
    /// The age binary. age-plugin-se must be on PATH for it.
    #[arg(long, env = "VAULT_AGE", default_value = "age")]
    age: PathBuf,
    /// The name-to-hash manifest vault-encrypt writes. A ciphertext it does
    /// not match is never decrypted.
    #[arg(long, env = "VAULT_MANIFEST")]
    manifest: PathBuf,
    #[command(flatten)]
    workos: WorkosArgs,
    /// Handle one pending request, waiting for one if need be, and exit: zero
    /// if it was approved or denied, otherwise non-zero. No reconnecting.
    #[arg(long)]
    once: bool,
}

/// How one request ended.
enum Outcome {
    Approved,
    Denied,
    /// Refused for the owner: it did not match, or it would not decrypt.
    Refused,
    /// Stdin ended at the prompt: nothing was answered.
    NoAnswer,
}

impl Outcome {
    /// What `--once` exits with.
    fn finish(self) -> anyhow::Result<()> {
        match self {
            Outcome::Approved | Outcome::Denied => Ok(()),
            Outcome::Refused => bail!("the request was refused, not sealed"),
            Outcome::NoAnswer => bail!("no answer"),
        }
    }
}

fn stamp(ms: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A macOS notification. The AppleScript is constant and the text is its
/// arguments: a purpose is someone else's string and AppleScript runs shell
/// commands. Elsewhere, or if it fails, the terminal prompt is still there.
fn notify(title: &str, body: &str) {
    let _ = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run", "--"])
        .args([title, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Discards anything typed before the question was on screen: a stray `y`
/// must not answer a request its owner has not read. After the prompt is out
/// and before the first read, so nothing typed before it can get through.
fn drop_typeahead() {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        // SAFETY: tcflush only takes the descriptor.
        unsafe { libc::tcflush(0, libc::TCIFLUSH) };
    }
}

/// `y` or `yes`: every other answer, and the end of input, is `false`.
fn ask() -> anyhow::Result<Option<bool>> {
    print!("Approve? [y/N] ");
    std::io::stdout().flush()?;
    drop_typeahead();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")))
}

/// `age -d -i identity | age -a -r key`, ciphertext on the first's stdin. The
/// value exists only in that pipe; what comes back is sealed to `key`.
async fn reseal(age: &Path, identity: &Path, ciphertext: String, key: &str) -> anyhow::Result<String> {
    // Checked again here: the server is not trusted to have checked, and this
    // string is about to be an argument.
    if !valid_one_time_pubkey(key) {
        bail!("the one-time key is not an X25519 age recipient");
    }
    let mut decrypt = tokio::process::Command::new(age)
        .arg("-d")
        .arg("-i")
        .arg(identity)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", age.display()))?;
    let pipe: Stdio = decrypt.stdout.take().context("no stdout")?.try_into().context("the pipe")?;
    let seal = tokio::process::Command::new(age)
        .arg("-a")
        .arg("-r")
        .arg(key)
        .stdin(pipe)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", age.display()))?;
    let mut stdin = decrypt.stdin.take().context("no stdin")?;
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(ciphertext.as_bytes()).await;
    });
    let both = async {
        let (decrypted, sealed) = tokio::join!(decrypt.wait(), seal.wait_with_output());
        anyhow::Ok((decrypted?, sealed?))
    };
    let (decrypted, sealed) = tokio::time::timeout(DECRYPT_TIMEOUT, both).await.context("age took too long")??;
    // A failed decrypt still leaves `age -r` a valid seal of nothing, so the
    // first exit status is the one that matters.
    if !decrypted.success() {
        bail!("age -d failed: {decrypted}");
    }
    if !sealed.status.success() {
        bail!("age -r failed: {}", sealed.status);
    }
    Ok(String::from_utf8(sealed.stdout).context("age's armored output is not text")?)
}

/// One call on its own connection, with a token good for now. The watch's
/// socket closes when its token ends, which can be under the prompt or during
/// Touch ID; an answer the owner has given must not go down with it.
async fn owner_call(args: &Args, login: &Login, method: &'static str, params: Value) -> anyhow::Result<VaultEvent> {
    let token = login.token().await?;
    let rpc = Rpc::connect(&args.server, &token).await?;
    rpc.call(method, params).await
}

/// Shows one request and acts on the answer.
async fn handle(args: &Args, login: &Login, request: &RequestSummary) -> anyhow::Result<Outcome> {
    let secret = printable(&request.secret, 64);
    let purpose = printable(&request.purpose, MAX_PURPOSE);
    let caller = printable(&request.caller, MAX_CALLER);
    let key = printable(&request.one_time_pubkey, 80);
    notify(&format!("vault: {secret}"), &format!("{purpose} ({caller})"));
    println!();
    println!("Request {}", printable(&request.id, 40));
    println!("  secret:  {secret}");
    println!("  purpose: {purpose}   [claimed by the caller]");
    println!("  caller:  {caller}   [claimed by the caller]");
    println!("  time:    {} (expires {})", stamp(request.created_at), stamp(request.expires_at));
    println!("  key:     …{}", fingerprint(&key));
    let answer = tokio::task::spawn_blocking(ask).await.context("the prompt")??;
    let Some(yes) = answer else { return Ok(Outcome::NoAnswer) };
    if !yes {
        owner_call(args, login, "deny", json!({"id": request.id})).await?;
        println!("denied");
        return Ok(Outcome::Denied);
    }
    // Fetched only now: it may have lapsed under the prompt, and then there is
    // no reason for Touch ID to fire.
    let (fresh, ciphertext) = match owner_call(args, login, "get_request", json!({"id": request.id})).await? {
        VaultEvent::Request { request, ciphertext } => (request, ciphertext),
        other => bail!("unexpected answer: {other:?}"),
    };
    // The approval was for what was on screen.
    if fresh != *request {
        bail!("the request changed under the prompt; nothing was decrypted");
    }
    // The approval was for this name's secret, whatever the server serves. No
    // Touch ID fires for bytes the manifest does not know.
    if let Err(e) = manifest::verify(&args.manifest, &request.secret, &ciphertext) {
        eprintln!("refused, nothing decrypted: {e:#}");
        owner_call(args, login, "deny", json!({"id": request.id})).await?;
        println!("denied");
        return Ok(Outcome::Refused);
    }
    match reseal(&args.age, &args.identity, ciphertext, &request.one_time_pubkey).await {
        Ok(sealed) => {
            owner_call(args, login, "fulfill", json!({"id": request.id, "sealed": sealed})).await?;
            println!("approved and sealed");
            Ok(Outcome::Approved)
        }
        // A cancelled Touch ID, a key that does not fit: the answer is no, and
        // the caller should not sit out the timeout.
        Err(e) => {
            eprintln!("not sealed: {e:#}");
            owner_call(args, login, "deny", json!({"id": request.id})).await?;
            println!("denied");
            Ok(Outcome::Refused)
        }
    }
}

/// One connection's life. `Ok`: stop. `Err`: reconnect, or under `--once`, exit.
async fn session(args: &Args, login: &Login, seen: &mut HashSet<String>) -> anyhow::Result<()> {
    // Asked for afresh on every reconnect: the socket ends with its token.
    let token = login.token().await?;
    let rpc = Rpc::connect(&args.server, &token).await?;
    println!("watching {}", args.server);
    let mut events = rpc.stream("watch", json!({}));
    while let Some(event) = events.next().await {
        match event {
            Ok(ClientEvent::Data(VaultEvent::Pending { request })) => {
                if !seen.insert(request.id.clone()) {
                    continue;
                }
                if request.expires_at <= now_ms() {
                    continue;
                }
                match handle(args, login, &request).await {
                    Ok(outcome) if args.once => return outcome.finish(),
                    Err(e) if args.once => return Err(e),
                    Ok(Outcome::NoAnswer) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => eprintln!("request {}: {e:#}", printable(&request.id, 40)),
                }
            }
            Ok(ClientEvent::Data(VaultEvent::Error { message })) => bail!("{}", printable(&message, 400)),
            Ok(_) => {}
            Err(e) => bail!("{e}"),
        }
    }
    bail!("the watch ended")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if !args.identity.exists() {
        bail!("no identity at {}", args.identity.display());
    }
    let login = args.workos.login()?;
    let mut seen = HashSet::new();
    loop {
        match session(&args, &login, &mut seen).await {
            Ok(()) => return Ok(()),
            Err(e) if args.once => return Err(e),
            Err(e) => {
                eprintln!("disconnected: {e:#}; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
