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

    // Per-app data root. Each session gets a project- and component-scoped
    // subdirectory underneath this:
    //
    //   <data_root>/<project_id>/<component_id>/    <-- mounted at /data
    //
    // `<project_id>` is a hash of the session's cwd (no path leakage in
    // the dir name); `<component_id>` is the wasm filename stem. The
    // result: data is naturally siloed per project so an agent can't
    // accidentally leak history between unrelated codebases.
    let data_root = resolve_data_root().context("resolving data root")?;
    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("creating data root {}", data_root.display()))?;
    info!(path = %data_root.display(), "data root");

    let component_id = component_id_from_path(&args.wasm_path)
        .context("deriving component id from wasm filename")?;
    info!(component = %component_id, "component id");

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
        let factory = Arc::new(SessionFactory::new(
            engine,
            component,
            outbound_tx,
            data_root,
            component_id,
        ));
        let registry = Arc::new(SessionRegistry::new());

        info!(path = %args.wasm_path.display(), "loaded wasm component");
        info!("listening for ACP JSON-RPC on stdio");

        bridge::run(factory, registry, outbound_rx).await
    })
}

/// `$XDG_STATE_HOME/playground-wasm-acp`, falling back to
/// `$HOME/.local/state/playground-wasm-acp`. This is the *root*; per-session
/// data dirs are subpaths underneath.
fn resolve_data_root() -> Result<PathBuf> {
    const APP: &str = "playground-wasm-acp";
    if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(base).join(APP));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local").join("state").join(APP))
}

/// Derive a component id from the wasm path. We use the file stem; renaming
/// the binary therefore loses prior data (acceptable for a sample, and a
/// future `--component-id` flag can override). Restricted to a small
/// alphabet to avoid surprising filesystem behaviour.
fn component_id_from_path(path: &std::path::Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("wasm path has no usable file stem: {}", path.display()))?;
    let ok = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        anyhow::bail!(
            "wasm filename stem {stem:?} contains characters not allowed in a component id (allow [A-Za-z0-9._-])"
        );
    }
    Ok(stem.to_string())
}
