//! The service. It binds one address: loopback by default, the Tailscale IP in
//! deployment, and refuses anything else unless told otherwise. It announces
//! itself to no registry.

use clap::Parser;
use anyhow::{bail, Context};
use plexus_auth_core::adapters::{OidcAdapter, WORKOS_API};
use plexus_core::plexus::DynamicHub as DynamicService;
use plexus_transport::websocket::serve_websocket_with;
use plexus_transport::{UpgradeOptions, WebSocketConfig};
use hyperforge_vault::owner::{bind_is_safe, workos_principal, MachineCredential, OwnerGate, VaultValidator};
use hyperforge_vault::server::{activation::VaultActivation, Store};
use hyperforge_vault::wire::SERVICE;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "hyperforge-vault", about = "The vault service: it stores age ciphertext and has no key")]
struct Args {
    /// The address to bind.
    #[arg(long, env = "VAULT_BIND", default_value = "127.0.0.1")]
    bind: IpAddr,
    #[arg(short, long, env = "VAULT_PORT", default_value = "4470")]
    port: u16,
    /// Where the ciphertexts, requests and audit log live.
    #[arg(long, env = "VAULT_STATE_DIR")]
    state_dir: PathBuf,
    /// How long a request waits for the owner.
    #[arg(long, env = "VAULT_REQUEST_TTL_SECS", default_value = "300")]
    request_ttl_secs: u64,
    /// How long a fulfilled result waits for its one read.
    #[arg(long, env = "VAULT_RESULT_TTL_SECS", default_value = "60")]
    result_ttl_secs: u64,
    /// Bind an address that is neither loopback nor a tailnet's. Bearer tokens
    /// cross the socket as ws://: this says something else protects them.
    #[arg(long)]
    allow_unsafe_bind: bool,
    /// The WorkOS client whose tokens are accepted. With --owner-user-id, or
    /// nobody is admitted.
    #[arg(long, env = "VAULT_WORKOS_CLIENT_ID")]
    workos_client_id: Option<String>,
    /// The WorkOS API; the issuer is <this>/user_management/<client id>.
    #[arg(long, env = "VAULT_WORKOS_BASE", default_value = WORKOS_API)]
    workos_base: String,
    /// The owner's WorkOS user id (`user_...`): the only person admitted, on a
    /// client other people can sign in to.
    #[arg(long, env = "VAULT_OWNER_USER_ID")]
    owner_user_id: Option<String>,
    /// A file of `<name> <sha256 of its token>` lines: the machines that may
    /// ask for a secret and collect the answer.
    #[arg(long, env = "VAULT_MACHINE_CREDENTIALS")]
    machine_credentials: Option<PathBuf>,
}

/// The door and the gate, from the configuration. Missing either half of the
/// WorkOS configuration is a server that refuses, not one that admits.
fn door(args: &Args) -> anyhow::Result<(VaultValidator, OwnerGate)> {
    let machines = match &args.machine_credentials {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            MachineCredential::parse_list(&text).with_context(|| format!("in {}", path.display()))?
        }
        None => Vec::new(),
    };
    let client = args.workos_client_id.as_deref().filter(|s| !s.is_empty());
    let owner = args.owner_user_id.as_deref().filter(|s| !s.is_empty());
    let (Some(client), Some(owner)) = (client, owner) else {
        tracing::warn!("no WorkOS client or no owner configured: every caller is refused");
        return Ok((VaultValidator::refusing(), OwnerGate::unconfigured()));
    };
    let oidc = OidcAdapter::workos(client, Some(&args.workos_base)).map_err(|e| anyhow::anyhow!("the WorkOS configuration: {e}"))?;
    let owner = workos_principal(owner)?;
    Ok((VaultValidator::new(oidc, owner.clone(), machines)?, OwnerGate::owned_by(owner)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // jsonrpsee logs each whole upgrade request at TRACE, Authorization header
    // and all: whatever RUST_LOG says, every bearer token stays out of the log.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into())
        .add_directive("jsonrpsee-server=info".parse()?);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if !bind_is_safe(args.bind) && !args.allow_unsafe_bind {
        bail!("{} is neither loopback nor a tailnet address: bearer tokens would cross it in the clear. Pass --allow-unsafe-bind if something else protects it", args.bind);
    }
    // Everything this process creates is the owner's alone.
    unsafe { libc::umask(0o077) };

    let store = Arc::new(Store::open(&args.state_dir, Duration::from_secs(args.request_ttl_secs), Duration::from_secs(args.result_ttl_secs))?);
    let (validator, owner) = door(&args)?;
    let service = Arc::new(DynamicService::new(SERVICE).register(VaultActivation::new(store.clone()).with_owner(owner)));
    let module = DynamicService::arc_into_rpc_module(service).map_err(|e| anyhow::anyhow!("rpc module: {e}"))?;

    let addr = SocketAddr::new(args.bind, args.port);
    let mut config = WebSocketConfig::new(addr.port());
    config.addr = addr;
    // No page has business here. An empty list would admit every origin; this
    // one matches none, while a client with no Origin (the tools) proceeds.
    config.allowed_origins = vec!["https://no-browser.invalid".into()];
    // No identity, no socket: 401 at the upgrade. Each method checks again.
    let upgrade = UpgradeOptions { reject_on_session_failure: true, ..UpgradeOptions::default() };
    let (handle, bound) = serve_websocket_with(module, config, Some(Arc::new(validator)), upgrade)
        .await
        .map_err(|e| anyhow::anyhow!("cannot serve on {addr}: {e}"))?;
    tracing::info!("{SERVICE} on ws://{bound}");

    let sweeper = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            if let Err(e) = sweeper.sweep() {
                tracing::error!("sweep: {e:#}");
            }
        }
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = handle.clone().stopped() => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    let _ = handle.stop();
    Ok(())
}
