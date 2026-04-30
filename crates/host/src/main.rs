//! ACP wasmtime host.
//!
//! Loads an ACP agent component and bridges it to the editor over the ACP
//! JSON-RPC wire protocol on stdio. Logs go to stderr; configure verbosity
//! with the `RUST_LOG` environment variable (e.g. `RUST_LOG=host=debug`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::info;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

mod bridge;
mod client_impl;
mod state;
mod translate;
mod wasm;

// Generate wasmtime component bindings for the `provider` world. From the
// host's perspective, bindgen flips imports/exports: the `client` interface
// becomes a Host trait we implement (see [`client_impl`]), and the `agent`
// interface becomes callable methods on the bindings struct (see [`wasm`]).
wasmtime::component::bindgen!({
    path: "../../vendor/wit",
    world: "provider",
    imports: { default: async },
    exports: { default: async },
});

use crate::wasm::{SessionFactory, SessionRegistry};

#[derive(Parser)]
struct Args {
    /// Path to the ACP agent wasm component.
    wasm_path: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("host=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &args.wasm_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading {}", args.wasm_path.display()))?;

    // Single-threaded runtime + `LocalSet`: lets us host `!Send` session
    // actors via `spawn_local` while the bridge's `Send`-bound handlers
    // (required by `agent_client_protocol::Builder`) cross the boundary
    // through `Send + Sync` channel handles.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local = LocalSet::new();
    local.block_on(&runtime, async move {
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let factory = Arc::new(SessionFactory::new(engine, component, outbound_tx));
        let registry = Arc::new(SessionRegistry::new());

        info!(path = %args.wasm_path.display(), "loaded wasm component");
        info!("listening for ACP JSON-RPC on stdio");

        bridge::run(factory, registry, outbound_rx).await
    })
}
