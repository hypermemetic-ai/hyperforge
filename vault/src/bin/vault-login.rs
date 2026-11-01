//! Signs the owner in to WorkOS and stores the tokens for `vault-approve` and
//! `vault-encrypt`. A public client: PKCE, loopback redirect, no secret. The
//! refresh token crosses this heap, so it gets what the other tools get.

#[path = "../platform.rs"]
mod platform;

use clap::Parser;
use hyperforge_vault::login::{sign_in, LoginRequest, WorkosArgs};
use std::process::{Command, Stdio};

/// Everything freed in this process is wiped: see `platform`.
#[global_allocator]
static HEAP: platform::Zeroing = platform::Zeroing(std::alloc::System);

#[derive(Parser, Debug)]
#[command(name = "vault-login", about = "Sign the vault's owner in to WorkOS")]
struct Args {
    #[command(flatten)]
    workos: WorkosArgs,
    /// Must be registered on the WorkOS client, byte for byte.
    #[arg(long, env = "VAULT_REDIRECT_URI", default_value = "http://127.0.0.1:4471/callback")]
    redirect_uri: String,
    /// `authkit`, or the provider the WorkOS environment signs in with.
    #[arg(long, env = "VAULT_WORKOS_PROVIDER", default_value = "authkit")]
    provider: String,
    /// Print the URL and leave the browser alone.
    #[arg(long)]
    no_browser: bool,
}

/// Best effort: the URL is on stdout either way.
fn open(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::undumpable()?;
    let args = Args::parse();
    let login = args.workos.login()?;
    let request = LoginRequest { redirect_uri: &args.redirect_uri, provider: &args.provider };
    let no_browser = args.no_browser;
    sign_in(&login, request, |url| if !no_browser { open(url) }).await
}
