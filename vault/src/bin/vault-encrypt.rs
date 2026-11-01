//! Encrypts stdin on this machine, in this process, and stores the ciphertext.
//! The plaintext moves through one zeroizing buffer and is held nowhere else.

#[path = "../platform.rs"]
mod platform;

use anyhow::{bail, Context};
use clap::Parser;
use hyperforge_vault::client::Rpc;
use hyperforge_vault::login::WorkosArgs;
use hyperforge_vault::{keys, manifest};
use hyperforge_vault::wire::{is_age_format, valid_secret_name};
use serde_json::json;
use std::os::fd::AsFd;
use std::path::PathBuf;

/// Everything freed in this process is wiped: see `platform`.
#[global_allocator]
static HEAP: platform::Zeroing = platform::Zeroing(std::alloc::System);

#[derive(Parser, Debug)]
#[command(name = "vault-encrypt", about = "Encrypt stdin to a recipients file, locally, and store the ciphertext")]
struct Args {
    /// The secret's name.
    name: String,
    /// An age recipients file, one recipient a line: as many as you like, and
    /// a recovery one among them.
    recipients_file: PathBuf,
    #[arg(long, env = "VAULT_URL", default_value = "ws://127.0.0.1:4470")]
    server: String,
    #[command(flatten)]
    workos: WorkosArgs,
    /// The approver's manifest: this ciphertext's hash is recorded here.
    #[arg(long, env = "VAULT_MANIFEST")]
    manifest: PathBuf,
    /// Accepted and redundant: memory that cannot be locked always stops this tool.
    #[arg(long = "require-memory-lock", hide = true)]
    _require_memory_lock: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::undumpable()?;
    let locked = platform::lock_heap();
    let args = Args::parse();
    // No value is read on a heap that may reach swap.
    platform::require_or_warn(locked, true)?;
    if !valid_secret_name(&args.name) {
        bail!("{:?} is not a secret name", args.name);
    }
    let text = std::fs::read_to_string(&args.recipients_file).with_context(|| format!("reading {}", args.recipients_file.display()))?;
    let recipients = keys::parse_recipients(&text, keys::FORMATS).with_context(|| format!("{}", args.recipients_file.display()))?;
    if recipients.is_empty() {
        bail!("{} names no recipient", args.recipients_file.display());
    }
    if recipients.len() == 1 {
        eprintln!("warning: one recipient: a lost Mac would lose this secret. A recovery recipient belongs in this file.");
    }
    // The descriptor, unbuffered: std's stdin keeps its own buffer, and that
    // one is never zeroed.
    let mut stdin = std::fs::File::from(std::io::stdin().as_fd().try_clone_to_owned().context("stdin")?);
    let ciphertext = keys::encrypt(&recipients, &mut stdin)?;
    platform::check_locks(true)?;
    if !is_age_format(&ciphertext) {
        bail!("the output does not parse as age");
    }
    let token = args.workos.login()?.token().await?;
    let rpc = Rpc::connect(&args.server, &token).await.context("connecting to the vault")?;
    rpc.call("put", json!({"name": args.name, "ciphertext": ciphertext})).await?;
    // After the server took it: a refused overwrite must not move the entry.
    manifest::record(&args.manifest, &args.name, &ciphertext)
        .with_context(|| format!("{} is stored but the approver will refuse it", args.name))?;
    println!("stored {}", args.name);
    Ok(())
}
