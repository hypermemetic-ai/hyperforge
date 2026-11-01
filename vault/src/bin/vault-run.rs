//! Runs a command with a secret in its environment. The value is decrypted in
//! this process's memory and handed to exec: never argv, never a file.

#[path = "../platform.rs"]
mod platform;

use anyhow::{bail, Context};
use clap::Parser;
use hyperforge_vault::client::{default_caller, fetch, Ask};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "vault-run", about = "Run a command with a secret in its environment, once the owner approves")]
struct Args {
    /// The secret's name.
    secret: String,
    /// Why: shown to the owner, who has no way to check it.
    #[arg(long)]
    purpose: String,
    /// The environment variable that carries the value.
    #[arg(long = "env", value_name = "NAME", default_value = "VAULT_SECRET")]
    env: String,
    #[arg(long, env = "VAULT_URL", default_value = "ws://127.0.0.1:4470")]
    server: String,
    /// Who is asking, as shown to the owner (default: user@host).
    #[arg(long)]
    caller: Option<String>,
    #[arg(long, default_value = "1000")]
    poll_ms: u64,
    /// Stop waiting after this long, whatever the server says.
    #[arg(long, default_value = "900")]
    max_wait_secs: u64,
    /// The command and its arguments.
    #[arg(last = true, required = true)]
    command: Vec<OsString>,
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::no_core_dumps()?;
    let args = Args::parse();
    if !valid_env_name(&args.env) {
        bail!("--env {:?} is not an environment variable name", args.env);
    }
    let caller = args.caller.clone().unwrap_or_else(default_caller);
    let value = fetch(Ask {
        url: &args.server,
        secret: &args.secret,
        purpose: &args.purpose,
        caller: &caller,
        poll: Duration::from_millis(args.poll_ms),
        max_wait: Duration::from_secs(args.max_wait_secs),
    })
    .await?;
    if value.contains(&0) {
        bail!("the value holds a NUL byte, which an environment variable cannot");
    }
    let mut command = Command::new(&args.command[0]);
    command.args(&args.command[1..]).env(&args.env, OsStr::from_bytes(&value));
    let error = command.exec();
    Err(error).with_context(|| format!("cannot run {:?}", args.command[0]))
}
