//! Hyperforge Auth Service
//!
//! Simple secret management service using YAML storage.
//!
//! Usage:
//!   hyperforge-auth [--port PORT]
//!
//! Default port: 44105 (lforge default + 1)

use clap::Parser;
use plexus_core::plexus::DynamicHub as DynamicService;
use plexus_transport::TransportServer;
use hyperforge::auth_service::AuthService;
use std::sync::Arc;
use tracing::{info, error};

#[derive(Parser, Debug)]
#[command(name = "hyperforge-auth")]
#[command(about = "Hyperforge Auth Service - Secret management", long_about = None)]
struct Args {
    /// Port to listen on (default: lforge port + 1 = 44105)
    #[arg(short, long, default_value = "44105")]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    let args = Args::parse();

    info!("Starting Hyperforge Auth Service");
    info!("Storage: ~/.config/hyperforge/secrets.yaml");

    // Create auth service
    let auth_service = match AuthService::new().await {
        Ok(service) => service,
        Err(e) => {
            error!("Failed to create auth service: {}", e);
            std::process::exit(1);
        }
    };

    info!("Auth service initialized");

    // Create auth service with unique namespace
    // Note: Using "secrets" as DynamicService namespace to avoid confusion with "auth" activation
    let auth = Arc::new(
        DynamicService::new("secrets")
            .register(auth_service)
    );

    info!("Auth service started");
    info!("  Namespace: secrets");
    info!("  Activation: auth");
    info!("  Version: 1.0.0");
    info!("  Description: Simple secret management");

    // Configure transport server
    let rpc_converter = |arc: Arc<DynamicService>| {
        DynamicService::arc_into_rpc_module(arc)
            .map_err(|e| anyhow::anyhow!("Failed to create RPC module: {e}"))
    };

    let builder = TransportServer::builder(auth, rpc_converter)
        .with_websocket(args.port);

    info!("Server started");
    info!("  WebSocket: ws://127.0.0.1:{}", args.port);
    info!("");
    info!("Usage:");
    info!("  synapse -P {} secrets auth set_secret --secret-key <KEY> --value <VALUE>", args.port);
    info!("  synapse -P {} secrets auth get_secret --secret-key <KEY>", args.port);
    info!("  synapse -P {} secrets auth list_secrets --prefix <PREFIX>", args.port);
    info!("");

    // Start the transport server
    builder.build().await?.serve().await
}
