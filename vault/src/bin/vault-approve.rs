//! The owner's side, on the Mac. It watches the vault, shows each request, and
//! on a yes opens the ciphertext and seals it to the caller's one-time key, here
//! in this process: the plaintext crosses one zeroizing buffer and goes no further.

#[path = "../platform.rs"]
mod platform;
#[path = "shared/workos.rs"]
mod workos;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use plexus_client::ClientEvent;
use hyperforge_vault::client::{fingerprint, Rpc};
use hyperforge_vault::login::Login;
use hyperforge_vault::{enclave, keys, manifest};
use workos::WorkosArgs;
use hyperforge_vault::wire::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

/// Everything freed in this process is wiped: see `platform`.
#[global_allocator]
static HEAP: platform::Zeroing = platform::Zeroing(std::alloc::System);

/// How long Touch ID may be left waiting before the attempt is abandoned.
const DECRYPT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Parser, Debug)]
#[command(name = "vault-approve", about = "Approve or deny vault requests; a yes decrypts here, with the identity", subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Tool>,
    /// The age identity file. Read at each decrypt, and held no longer.
    #[arg(long, env = "VAULT_IDENTITY", required = true)]
    identity: Option<PathBuf>,
    #[arg(long, env = "VAULT_URL", default_value = "ws://127.0.0.1:4470")]
    server: String,
    /// The name-to-hash manifest vault-encrypt writes. A ciphertext it does
    /// not match is never decrypted.
    #[arg(long, env = "VAULT_MANIFEST", required = true)]
    manifest: Option<PathBuf>,
    #[command(flatten)]
    workos: WorkosArgs,
    /// Handle one pending request, waiting for one if need be, and exit: zero
    /// if it was approved or denied, otherwise non-zero. No reconnecting.
    #[arg(long)]
    once: bool,
    /// Accepted and redundant: watching stops at startup if memory cannot be
    /// locked out of swap, and sends no seal for a request whose blocks fail to lock.
    #[arg(long = "require-memory-lock", hide = true)]
    _require_memory_lock: bool,
}

#[derive(Subcommand, Debug)]
enum Tool {
    /// Write a new recovery identity to a new file, and print its public
    /// recipient. The secret goes to the file and nowhere else.
    RecoveryKeygen {
        /// Where to write it. Refused if something is there.
        #[arg(long)]
        out: PathBuf,
    },
    /// Make the owner's key in this Mac's Secure Enclave, write its identity to a
    /// new file, and print its public recipient. Touch ID does not fire: it fires
    /// at each decrypt, and only the fingerprints enrolled now will open it.
    Enrol {
        /// Where to write it. Refused if something is there.
        #[arg(long)]
        out: PathBuf,
    },
    /// Say whether an enclave identity passes the gate, and print its recipient.
    /// Uses the key for nothing, so Touch ID does not fire.
    EnrolCheck {
        #[arg(long)]
        identity: PathBuf,
    },
    /// Open one stored ciphertext with the offline recovery identity, and print
    /// the value to stdout. Stdout only: nothing here writes a file. No server
    /// is needed, and the ciphertext must match the manifest.
    Recover {
        /// The secret's name, as it is in the manifest.
        name: String,
        /// A copy of the server's `secrets/<name>.age`.
        #[arg(long)]
        ciphertext: PathBuf,
        /// The recovery identity. Named every time: no environment variable, so
        /// the owner's own identity is not picked up by accident.
        #[arg(long)]
        identity: PathBuf,
        /// A copy of the manifest. A ciphertext it does not match is not opened.
        #[arg(long, env = "VAULT_MANIFEST")]
        manifest: PathBuf,
        /// Print even though stdout is a terminal, where it stays in the scrollback.
        #[arg(long)]
        allow_terminal: bool,
        /// Stop, before opening anything, if memory cannot be locked out of swap.
        /// Otherwise a warning: a recovery on a bad day must still work.
        #[arg(long)]
        require_memory_lock: bool,
    },
}

/// What watching needs: [`Args`], with the requirements clap checked.
#[derive(Debug)]
struct Watch {
    identity: PathBuf,
    server: String,
    manifest: PathBuf,
    workos: WorkosArgs,
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

/// One call on its own connection, with a token good for now. The watch's
/// socket closes when its token ends, which can be under the prompt or during
/// Touch ID; an answer the owner has given must not go down with it.
async fn owner_call(args: &Watch, login: &Login, method: &'static str, params: Value) -> anyhow::Result<VaultEvent> {
    let token = login.token().await?;
    let rpc = Rpc::connect(&args.server, &token).await?;
    rpc.call(method, params).await
}

/// Shows one request and acts on the answer.
async fn handle(args: &Watch, login: &Login, request: &RequestSummary) -> anyhow::Result<Outcome> {
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
    let (identity, key) = (args.identity.clone(), request.one_time_pubkey.clone());
    // This request's blocks and no others: see `LockWatch`.
    let locks = platform::LockWatch::begin();
    let sealed = keys::with_deadline(DECRYPT_TIMEOUT, move || keys::reseal(&identity, keys::FORMATS, &ciphertext, &key)).await;
    let sealed = sealed.and_then(|sealed| locks.check(true).map(|()| sealed));
    match sealed {
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
async fn session(args: &Watch, login: &Login, seen: &mut HashSet<String>) -> anyhow::Result<()> {
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

/// Prints one recovered value. The descriptor, unbuffered: std's stdout keeps its
/// own buffer, which lives as long as the process and is never freed, so never
/// wiped. Checked before anything is read: no use unlocking a key for this.
fn recover(name: &str, ciphertext: &Path, identity: &Path, manifest: &Path, allow_terminal: bool) -> anyhow::Result<()> {
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned().context("stdout")?);
    if stdout.is_terminal() && !allow_terminal {
        bail!("stdout is a terminal; pipe it, or pass --allow-terminal");
    }
    let mut text = String::new();
    let file = std::fs::File::open(ciphertext).with_context(|| format!("opening {}", ciphertext.display()))?;
    file.take(MAX_CIPHERTEXT as u64 + 1).read_to_string(&mut text).with_context(|| format!("{} is not armored text", ciphertext.display()))?;
    if text.len() > MAX_CIPHERTEXT {
        bail!("{} is larger than any stored secret", ciphertext.display());
    }
    let value = keys::recover(manifest, name, identity, keys::FORMATS, &text)?;
    stdout.write_all(&value).context("writing to stdout")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::undumpable()?;
    let locked = platform::lock_heap();
    let args = Args::parse();
    match &args.command {
        Some(Tool::RecoveryKeygen { out }) => {
            // Written once and left on paper: a warning is all this needs.
            platform::require_or_warn(locked, false)?;
            let recipient = keys::generate_recovery_identity(out)?;
            eprintln!("wrote the recovery identity to {}; its public recipient follows", out.display());
            println!("{recipient}");
            return Ok(());
        }
        Some(Tool::Enrol { out }) => {
            // Holds no value, and the key is not in this process: a warning is enough.
            platform::require_or_warn(locked, false)?;
            let recipient = keys::enrol(out, &enclave::ThisMachine)?;
            eprintln!("wrote the enclave identity to {}; its public recipient follows", out.display());
            println!("{recipient}");
            return Ok(());
        }
        Some(Tool::EnrolCheck { identity }) => {
            platform::require_or_warn(locked, false)?;
            let recipient = keys::check_enrolment(identity, &enclave::ThisMachine)?;
            eprintln!("{} needs the current fingerprints: it passes", identity.display());
            println!("{recipient}");
            return Ok(());
        }
        Some(Tool::Recover { name, ciphertext, identity, manifest, allow_terminal, require_memory_lock }) => {
            platform::require_or_warn(locked, *require_memory_lock)?;
            return recover(name, ciphertext, identity, manifest, *allow_terminal);
        }
        // No request is answered on a heap that may reach swap.
        None => platform::require_or_warn(locked, true)?,
    }
    let (Some(identity), Some(manifest)) = (args.identity, args.manifest) else {
        bail!("--identity and --manifest are required");
    };
    let args = Watch { identity, server: args.server, manifest, workos: args.workos, once: args.once };
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
