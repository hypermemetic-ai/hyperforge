//! Where the owner signs in, as the tools take it on their command line. This
//! lives with the binaries because the library is what a mobile build shares,
//! and a phone has neither clap nor `HOME`.

use hyperforge_vault::login::{Login, WORKOS_API};
use std::path::PathBuf;

/// All of it public; the deployment's values live in `config/production.env`.
#[derive(clap::Args, Debug, Clone)]
pub struct WorkosArgs {
    /// The WorkOS client's public id. An `Option` only so that a tool's offline
    /// subcommands, which `subcommand_negates_reqs` frees of it, still parse:
    /// `vault-approve recover` needs no sign-in. Clap requires it everywhere else.
    #[arg(long, env = "VAULT_WORKOS_CLIENT_ID", required = true)]
    pub workos_client_id: Option<String>,
    /// The WorkOS API.
    #[arg(long, env = "VAULT_WORKOS_BASE", default_value = WORKOS_API)]
    pub workos_base: String,
    /// Where `vault-login` keeps the owner's tokens, owner-only.
    #[arg(long, env = "VAULT_LOGIN_FILE")]
    pub login_file: Option<PathBuf>,
}

impl WorkosArgs {
    fn login_path(&self) -> anyhow::Result<PathBuf> {
        match &self.login_file {
            Some(path) => Ok(path.clone()),
            None => Ok(PathBuf::from(std::env::var("HOME").map_err(|_| anyhow::anyhow!("no HOME: pass --login-file"))?).join(".config/vault/login.json")),
        }
    }

    pub fn login(&self) -> anyhow::Result<Login> {
        let client_id = self.workos_client_id.clone().ok_or_else(|| anyhow::anyhow!("no WorkOS client id: pass --workos-client-id"))?;
        Login::new(client_id, &self.workos_base, self.login_path()?)
    }
}
