//! The service. It binds one address: loopback by default, the Tailscale IP in
//! deployment. It announces itself to no registry.

use clap::Parser;
use plexus_core::plexus::DynamicHub as DynamicService;
use plexus_transport::websocket::serve_websocket_with;
use plexus_transport::{UpgradeOptions, WebSocketConfig};
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    // Everything this process creates is the owner's alone.
    unsafe { libc::umask(0o077) };

    let store = Arc::new(Store::open(&args.state_dir, Duration::from_secs(args.request_ttl_secs), Duration::from_secs(args.result_ttl_secs))?);
    let service = Arc::new(DynamicService::new(SERVICE).register(VaultActivation::new(store.clone())));
    let module = DynamicService::arc_into_rpc_module(service).map_err(|e| anyhow::anyhow!("rpc module: {e}"))?;

    let addr = SocketAddr::new(args.bind, args.port);
    let mut config = WebSocketConfig::new(addr.port());
    config.addr = addr;
    // No page has business here. An empty list would admit every origin; this
    // one matches none, while a client with no Origin (the tools) proceeds.
    config.allowed_origins = vec!["https://no-browser.invalid".into()];
    let (handle, bound) = serve_websocket_with(module, config, None, UpgradeOptions::default())
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
