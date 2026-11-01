//! Prints a secret to stdout: into a pipe, or to a terminal only when told.

#[path = "../platform.rs"]
mod platform;

use anyhow::bail;
use clap::Parser;
use hyperforge_vault::client::{default_caller, fetch, Ask};
use std::io::{IsTerminal, Write};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "vault-get", about = "Print a secret once the owner approves; a terminal needs --allow-terminal")]
struct Args {
    /// The secret's name.
    secret: String,
    /// Why: shown to the owner, who has no way to check it.
    #[arg(long)]
    purpose: String,
    /// Print even though stdout is a terminal, where it stays in the scrollback.
    #[arg(long)]
    allow_terminal: bool,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::no_core_dumps()?;
    let args = Args::parse();
    let mut stdout = std::io::stdout().lock();
    // Before the request: there is no use troubling the owner for this.
    if stdout.is_terminal() && !args.allow_terminal {
        bail!("stdout is a terminal; pipe it, or pass --allow-terminal");
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
    stdout.write_all(&value)?;
    stdout.flush()?;
    Ok(())
}
