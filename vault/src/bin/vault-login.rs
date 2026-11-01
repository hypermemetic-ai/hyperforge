//! Signs the owner in to WorkOS and stores the tokens for `vault-approve` and
//! `vault-encrypt`. A public client: the device flow, no callback, no secret. The
//! refresh token crosses this heap, so it gets what the other tools get.

#[path = "../platform.rs"]
mod platform;

use clap::Parser;
use hyperforge_vault::login::sign_in;
use std::process::{Command, Stdio};

#[path = "shared/workos.rs"]
mod workos;

/// Everything freed in this process is wiped: see `platform`.
#[global_allocator]
static HEAP: platform::Zeroing = platform::Zeroing(std::alloc::System);

#[derive(Parser, Debug)]
#[command(name = "vault-login", about = "Sign the vault's owner in to WorkOS")]
struct Args {
    #[command(flatten)]
    workos: workos::WorkosArgs,
    /// The owner's WorkOS user id: any other account's sign-in is refused.
    #[arg(long, env = "VAULT_OWNER_USER_ID")]
    owner_user_id: String,
    /// Print the URL and leave the browser alone.
    #[arg(long)]
    no_browser: bool,
}

/// Best effort: the URL is on stdout either way.
fn open(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    platform::undumpable()?;
    let args = Args::parse();
    let login = args.workos.login()?;
    let no_browser = args.no_browser;
    sign_in(&login, &args.owner_user_id, |prompt| {
        println!("{}", prompt.instructions());
        if let Some(url) = prompt.open.as_deref().filter(|_| !no_browser) {
            open(url);
        }
    })
    .await?;
    println!("signed in");
    Ok(())
}
