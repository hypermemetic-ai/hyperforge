//! Encrypts stdin on this machine and stores the ciphertext. `age` reads stdin
//! itself: the plaintext never passes through this process.

use anyhow::{bail, Context};
use clap::Parser;
use hyperforge_vault::client::Rpc;
use hyperforge_vault::manifest;
use hyperforge_vault::wire::{is_age_format, valid_secret_name};
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Parser, Debug)]
#[command(name = "vault-encrypt", about = "Encrypt stdin to a recipients file, locally, and store the ciphertext")]
struct Args {
    /// The secret's name.
    name: String,
    /// An age recipients file: the Secure Enclave recipient and the recovery one.
    recipients_file: PathBuf,
    #[arg(long, env = "VAULT_URL", default_value = "ws://127.0.0.1:4470")]
    server: String,
    /// The age binary.
    #[arg(long, env = "VAULT_AGE", default_value = "age")]
    age: PathBuf,
    /// The approver's manifest: this ciphertext's hash is recorded here.
    #[arg(long, env = "VAULT_MANIFEST")]
    manifest: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if !valid_secret_name(&args.name) {
        bail!("{:?} is not a secret name", args.name);
    }
    let recipients = std::fs::read_to_string(&args.recipients_file).with_context(|| format!("reading {}", args.recipients_file.display()))?;
    let count = recipients.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).count();
    if count == 0 {
        bail!("{} names no recipient", args.recipients_file.display());
    }
    if count == 1 {
        eprintln!("warning: one recipient: a lost Mac would lose this secret. A recovery recipient belongs in this file.");
    }
    let output = Command::new(&args.age)
        .arg("-a")
        .arg("-R")
        .arg(&args.recipients_file)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {}", args.age.display()))?;
    if !output.status.success() {
        bail!("age failed: {}", output.status);
    }
    let ciphertext = String::from_utf8(output.stdout).context("age's armored output is not text")?;
    if !is_age_format(&ciphertext) {
        bail!("age's output does not parse as age");
    }
    let rpc = Rpc::connect(&args.server).await.context("connecting to the vault")?;
    rpc.call("put", json!({"name": args.name, "ciphertext": ciphertext})).await?;
    // After the server took it: a refused overwrite must not move the entry.
    manifest::record(&args.manifest, &args.name, &ciphertext)
        .with_context(|| format!("{} is stored but the approver will refuse it", args.name))?;
    println!("stored {}", args.name);
    Ok(())
}
