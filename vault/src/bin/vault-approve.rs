//! The owner's side, on the Mac. It watches the vault, shows each request, and
//! on a yes opens the ciphertext and seals it to the caller's one-time key, here
//! in this process: the plaintext crosses one zeroizing buffer and goes no further.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use plexus_client::ClientEvent;
use hyperforge_vault::client::{fingerprint, no_core_dumps, read_credential, Rpc};
use hyperforge_vault::{keys, manifest};
use hyperforge_vault::wire::*;
use serde_json::json;
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

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
    /// A file holding the owner's credential.
    #[arg(long, env = "VAULT_OWNER_CREDENTIAL_FILE", required = true)]
    owner_credential_file: Option<PathBuf>,
    /// Handle one pending request, waiting for one if need be, and exit: zero
    /// if it was approved or denied, otherwise non-zero. No reconnecting.
    #[arg(long)]
    once: bool,
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
}

/// What watching needs: [`Args`], with the requirements clap checked.
#[derive(Debug)]
struct Watch {
    identity: PathBuf,
    server: String,
    manifest: PathBuf,
    owner_credential_file: PathBuf,
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

/// Shows one request and acts on the answer.
async fn handle(args: &Watch, rpc: &Rpc, request: &RequestSummary) -> anyhow::Result<Outcome> {
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
        rpc.call("deny", json!({"id": request.id})).await?;
        println!("denied");
        return Ok(Outcome::Denied);
    }
    // Fetched only now: it may have lapsed under the prompt, and then there is
    // no reason for Touch ID to fire.
    let (fresh, ciphertext) = match rpc.call("get_request", json!({"id": request.id})).await? {
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
        rpc.call("deny", json!({"id": request.id})).await?;
        println!("denied");
        return Ok(Outcome::Refused);
    }
    let (identity, key) = (args.identity.clone(), request.one_time_pubkey.clone());
    let sealed = keys::with_deadline(DECRYPT_TIMEOUT, move || keys::reseal(&identity, keys::FORMATS, &ciphertext, &key)).await;
    match sealed {
        Ok(sealed) => {
            rpc.call("fulfill", json!({"id": request.id, "sealed": sealed})).await?;
            println!("approved and sealed");
            Ok(Outcome::Approved)
        }
        // A cancelled Touch ID, a key that does not fit: the answer is no, and
        // the caller should not sit out the timeout.
        Err(e) => {
            eprintln!("not sealed: {e:#}");
            rpc.call("deny", json!({"id": request.id})).await?;
            println!("denied");
            Ok(Outcome::Refused)
        }
    }
}

/// One connection's life. `Ok`: stop. `Err`: reconnect, or under `--once`, exit.
async fn session(args: &Watch, credential: &str, seen: &mut HashSet<String>) -> anyhow::Result<()> {
    let rpc = Rpc::connect_as_owner(&args.server, credential).await?;
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
                match handle(args, &rpc, &request).await {
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
    no_core_dumps();
    let args = Args::parse();
    if let Some(Tool::RecoveryKeygen { out }) = &args.command {
        let recipient = keys::generate_recovery_identity(out)?;
        eprintln!("wrote the recovery identity to {}; its public recipient follows", out.display());
        println!("{recipient}");
        return Ok(());
    }
    let (Some(identity), Some(manifest), Some(owner_credential_file)) = (args.identity, args.manifest, args.owner_credential_file) else {
        bail!("--identity, --manifest and --owner-credential-file are required");
    };
    let args = Watch { identity, server: args.server, manifest, owner_credential_file, once: args.once };
    if !args.identity.exists() {
        bail!("no identity at {}", args.identity.display());
    }
    let credential = read_credential(&args.owner_credential_file)?;
    let mut seen = HashSet::new();
    loop {
        match session(&args, &credential, &mut seen).await {
            Ok(()) => return Ok(()),
            Err(e) if args.once => return Err(e),
            Err(e) => {
                eprintln!("disconnected: {e:#}; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
