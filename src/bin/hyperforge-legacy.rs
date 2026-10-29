use clap::Parser;
use plexus_core::plexus::DynamicHub as DynamicService;
use plexus_transport::TransportServer;
use hyperforge::HyperforgeService;
use hyperforge::auth_service::AuthService;
use hyperforge::registry::{RegistryClient, RegistryConfig};
use std::sync::Arc;

/// CLI arguments for hyperforge standalone server
#[derive(Parser, Debug)]
#[command(name = "hyperforge")]
#[command(about = "Hyperforge standalone server - JSON-RPC over WebSocket or stdio")]
struct Args {
    /// Run in stdio mode for MCP compatibility (line-delimited JSON-RPC over stdin/stdout)
    #[arg(long)]
    stdio: bool,

    /// Port for WebSocket server (ignored in stdio mode)
    #[arg(short, long, default_value = "44104")]
    port: u16,

    /// Enable MCP HTTP server (on port + 1)
    #[arg(long)]
    mcp: bool,

    /// Skip registry registration on startup
    #[arg(long)]
    no_register: bool,

    /// Skip auto-starting the secrets (hyperforge-auth) sidecar
    #[arg(long)]
    no_secrets: bool,

    /// Port where the Plexus registry is listening
    #[arg(long, default_value = "4444")]
    registry_port: u16,

    /// Name to register as in the registry
    #[arg(long, default_value = "lforge")]
    registry_name: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI arguments
    let args = Args::parse();

    // Initialize tracing with filtering
    // Logging targets (filterable via RUST_LOG):
    //   hyperforge                       — top-level startup/shutdown
    //   hyperforge::docker::connect      — Docker socket discovery
    //   hyperforge::docker::build        — image builds (trace = stream output)
    //   hyperforge::docker::push         — registry push (trace = layer progress)
    //   hyperforge::services::workspace      — sync/diff pipeline phases
    //   hyperforge::services::images         — container image operations
    //   hyperforge::adapters::registry   — registry API calls
    //
    // Examples:
    //   RUST_LOG="hyperforge=info"                              — normal ops
    //   RUST_LOG="hyperforge::docker=debug,hyperforge=info"     — debug Docker only
    //   RUST_LOG="hyperforge::docker::build=trace"              — stream build output
    let filter = if args.stdio {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("hyperforge=warn,jsonrpsee=warn")
        })
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            #[cfg(debug_assertions)]
            let default_filter = "warn,hyperforge=trace";
            #[cfg(not(debug_assertions))]
            let default_filter = "warn,hyperforge=info";
            tracing_subscriber::EnvFilter::new(default_filter)
        })
    };

    // In stdio mode, send logs to stderr to keep stdout clean for JSON-RPC
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting hyperforge at {}", chrono::Utc::now());

    // Create lforge service (DynamicService with "lforge" namespace, hyperforge activation registered)
    let lforge = Arc::new(
        DynamicService::new("lforge")
            .register(HyperforgeService::new())
    );

    // Log activation info
    tracing::info!("LFORGE2 initialized");
    tracing::info!("  Namespace: lforge");
    tracing::info!("  Activation: hyperforge");
    tracing::info!("  Version: {}", env!("CARGO_PKG_VERSION"));
    tracing::info!("  Description: Multi-forge repository management");

    // Registry registration (non-fatal — server starts regardless)
    let registry_client = if !args.no_register && !args.stdio {
        let config = RegistryConfig {
            registry_port: args.registry_port,
            name: args.registry_name.clone(),
            host: "127.0.0.1".into(),
            port: args.port,
            description: "Multi-forge repository management".into(),
            namespace: "lforge".into(),
        };
        let client = RegistryClient::new(config);

        match client.register().await {
            Ok(()) => {
                tracing::info!(
                    "registered as '{}' with registry at port {}",
                    args.registry_name,
                    args.registry_port,
                );
                Some(client)
            }
            Err(e) => {
                tracing::warn!("registry registration failed (non-fatal): {e}");
                Some(client)
            }
        }
    } else {
        None
    };

    // Auto-start secrets sidecar in-process on port + 1
    let secrets_handle = if !args.no_secrets && !args.stdio {
        let secrets_port = args.port + 1;
        match AuthService::new().await {
            Ok(auth_service) => {
                let secrets = Arc::new(
                    DynamicService::new("secrets")
                        .register(auth_service)
                );
                let rpc_conv = |arc: Arc<DynamicService>| {
                    DynamicService::arc_into_rpc_module(arc)
                        .map_err(|e| anyhow::anyhow!("Failed to create RPC module: {e}"))
                };
                match TransportServer::builder(secrets, rpc_conv)
                    .with_websocket(secrets_port)
                    .build()
                    .await
                {
                    Ok(server) => {
                        tracing::info!("secrets sidecar listening on port {secrets_port}");
                        Some(tokio::spawn(async move {
                            if let Err(e) = server.serve().await {
                                tracing::warn!("secrets sidecar exited: {e}");
                            }
                        }))
                    }
                    Err(e) => {
                        tracing::warn!("failed to start secrets sidecar (non-fatal): {e}");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to init secrets service (non-fatal): {e}");
                None
            }
        }
    } else {
        None
    };

    // Configure transport server
    let rpc_converter = |arc: Arc<DynamicService>| {
        DynamicService::arc_into_rpc_module(arc)
            .map_err(|e| anyhow::anyhow!("Failed to create RPC module: {e}"))
    };

    let mut builder = TransportServer::builder(lforge, rpc_converter);

    // Add requested transports
    if args.stdio {
        builder = builder.with_stdio();
    } else {
        builder = builder.with_websocket(args.port);

        if args.mcp {
            builder = builder.with_mcp_http(args.port + 1);
        }
    }

    // Log what we're starting
    if args.stdio {
        tracing::info!("Starting stdio transport (MCP-compatible)");
    } else {
        tracing::info!("LFORGE2 started");
        tracing::info!("  WebSocket: ws://127.0.0.1:{}", args.port);
        if args.mcp {
            tracing::info!("  MCP HTTP:  http://127.0.0.1:{}/mcp", args.port + 1);
        }
        if registry_client.is_some() {
            tracing::info!("  Registry:  port {} as '{}'", args.registry_port, args.registry_name);
        }
        tracing::info!("");
        tracing::info!("Usage:");
        tracing::info!("  synapse -p {} lforge hyperforge status", args.port);
        tracing::info!("  synapse -p {} lforge hyperforge version", args.port);
    }

    // Start the transport server with graceful shutdown
    let server = builder.build().await?;

    let result = tokio::select! {
        res = server.serve() => res,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received ctrl-c, shutting down");
            Ok(())
        }
    };

    // Abort secrets sidecar on shutdown
    if let Some(handle) = secrets_handle {
        handle.abort();
    }

    // Best-effort deregistration on shutdown
    if let Some(client) = registry_client {
        if let Err(e) = client.deregister().await {
            tracing::warn!("registry deregistration failed: {e}");
        }
    }

    result
}
