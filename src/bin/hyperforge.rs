//! hyperforge — canonical v5 daemon binary (V5PARITY-32).
//!
//! Listens on `--port` (default 44104, pinned in CONTRACTS D1 since
//! v5 became the canonical hyperforge in 5.0.0), registers a Plexus
//! `DynamicHub` namespaced as `lforge-v5` (D1), and serves
//! `HyperforgeHub` over WebSocket JSON-RPC.
//!
//! v4 still builds as `hyperforge-legacy` for one release as a
//! migration courtesy; will be removed in 6.0.0.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use plexus_core::plexus::DynamicHub;
use plexus_transport::TransportServer;

use hyperforge::v5::hub::HyperforgeHub;

/// CLI arguments for the hyperforge daemon.
#[derive(Parser, Debug)]
#[command(name = "hyperforge")]
#[command(about = "Hyperforge daemon — WebSocket JSON-RPC on --port")]
struct Args {
    /// Port for the WebSocket server. Defaults to 44104 (CONTRACTS D1).
    #[arg(short, long, default_value = "44104")]
    port: u16,

    /// Config directory. Defaults to `~/.config/hyperforge/`. Created if
    /// missing.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// State directory: pipeline runs, attempts, outputs and logs
    /// (HYPERFORGE-22). Defaults to `~/.local/state/hyperforge/`
    /// (`$XDG_STATE_HOME/hyperforge`). Created if missing.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .map_or_else(
            || PathBuf::from("/tmp/hyperforge"),
            |c| c.join("hyperforge"),
        )
}

fn default_state_dir(config_dir: &std::path::Path) -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map_or_else(|| config_dir.join("state"), |d| d.join("hyperforge"))
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,hyperforge=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Resolve and ensure config dir.
    let config_dir = args
        .config_dir
        .map_or_else(default_config_dir, |p| expand_tilde(&p));
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        eprintln!(
            "hyperforge-v5: failed to create config dir {}: {e}",
            config_dir.display()
        );
        std::process::exit(1);
    }

    // Pre-bind check: if the port is already in use, fail with a clear
    // diagnostic naming the port. Transport server does this too but its
    // error path is less test-friendly.
    match std::net::TcpListener::bind(("127.0.0.1", args.port)) {
        Ok(l) => drop(l),
        Err(e) => {
            eprintln!(
                "hyperforge-v5: failed to bind port {}: {e}",
                args.port
            );
            std::process::exit(1);
        }
    }

    // Resolve config_dir to its canonical absolute form. `canonicalize`
    // requires the path to exist, which `create_dir_all` above guarantees.
    let config_dir = match std::fs::canonicalize(&config_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "hyperforge-v5: failed to resolve config dir {}: {e}",
                config_dir.display()
            );
            std::process::exit(1);
        }
    };

    let state_dir = args
        .state_dir
        .map_or_else(|| default_state_dir(&config_dir), |p| expand_tilde(&p));
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        eprintln!(
            "hyperforge-v5: failed to create state dir {}: {e}",
            state_dir.display()
        );
        std::process::exit(1);
    }

    // Build the activation tree.
    let lforge = Arc::new(
        DynamicHub::new("lforge-v5")
            .register(HyperforgeHub::with_state_dir(config_dir.clone(), &state_dir)),
    );

    tracing::info!(
        port = args.port,
        config_dir = %config_dir.display(),
        state_dir = %state_dir.display(),
        "hyperforge-v5 starting"
    );

    let rpc_converter = |arc: Arc<DynamicHub>| {
        DynamicHub::arc_into_rpc_module(arc)
            .map_err(|e| anyhow::anyhow!("Failed to create RPC module: {e}"))
    };

    let server = TransportServer::builder(lforge, rpc_converter)
        .with_websocket(args.port)
        .build()
        .await?;

    tokio::select! {
        res = server.serve() => res,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
