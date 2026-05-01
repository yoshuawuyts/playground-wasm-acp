//! ACP wasmtime host.
//!
//! Loads an ACP agent component and bridges it to the editor over the ACP
//! JSON-RPC wire protocol on stdio. Logs go to stderr; configure verbosity
//! with the `RUST_LOG` environment variable (e.g. `RUST_LOG=host=debug`).
//! Pass `--log-file <path>` to also write logs to a file (useful for
//! debugging when stderr is hidden behind the editor).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
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

    /// Optional path to a file to mirror logs into. The same events that
    /// go to stderr are appended to this file (no ANSI colors). Useful
    /// when running under an editor that swallows or hides the host's
    /// stderr.
    #[arg(long)]
    log_file: Option<PathBuf>,

    /// Coarse log level. Equivalent to `RUST_LOG=host=<level>`. Use
    /// `--log-filter` for full `tracing` directive syntax (per-target
    /// levels). `RUST_LOG`, if set, takes precedence over both flags.
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    log_level: LogLevel,

    /// Full `tracing-subscriber` env-filter directive. Overrides
    /// `--log-level` when set. Example:
    /// `--log-filter "host=debug,agent_client_protocol=trace"`.
    #[arg(long)]
    log_filter: Option<String>,
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Verbosity: `RUST_LOG` wins if set (so existing habits keep working);
    // otherwise we honour `--log-filter` (full directive syntax) and finally
    // fall back to `--log-level` mapped to `host=<level>`.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            let directive = args
                .log_filter
                .clone()
                .unwrap_or_else(|| format!("host={}", args.log_level.as_str()));
            tracing_subscriber::EnvFilter::new(directive)
        });

    // Stderr layer is always present.
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    // File layer is opt-in via `--log-file`. We open in append mode so a
    // new run extends an existing file rather than truncating it. ANSI
    // colors are off so the file is grep-friendly.
    let file_layer = if let Some(path) = args.log_file.as_deref() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating log directory {}", parent.display())
                })?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    if let Some(path) = args.log_file.as_deref() {
        info!(path = %path.display(), "mirroring logs to file");
    }

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
