//! A toy service serving the contract over a scripted driver, for `synapse -s`
//! and `synapse-cc` (HYPERFORGE-9):
//!
//! ```sh
//! cargo run -p hyperforge-driver --example toy_service -- 44190
//! synapse -P 44190 -s toy driver
//! synapse -P 44190 toy driver trigger --target.repo.org hypermemetic --target.repo.name menger --target.name test --action deploy --reason try
//! ```

#[path = "../tests/support/scripted.rs"]
mod scripted;

use std::sync::Arc;

use hyperforge_driver::Driver;
use plexus_core::DynamicHub as DynamicService;
use plexus_transport::TransportServer;

#[tokio::main]
async fn main() -> anyhow_free::Result {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(44190);
    let service = Arc::new(DynamicService::new("toy").register(scripted::Scripted::new().server()));
    let server = TransportServer::builder(service, |h| DynamicService::arc_into_rpc_module(h).map_err(Into::into))
        .with_websocket(port)
        // HYPERFORGE-65: no browser page, and only this port's loopback names.
        .with_allowed_origins(["https://no-browser.invalid"])
        .with_allowed_hosts(plexus_transport::host::loopback_hosts(port))
        .without_registry()
        .build()
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("toy service: the driver contract on ws://127.0.0.1:{port}");
    server.serve().await.map_err(|e| e.to_string())
}

mod anyhow_free {
    pub type Result = std::result::Result<(), String>;
}
