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
mod utils;
mod wasm;

// Generate wasmtime component bindings for both ACP worlds.
//
// The `layer` world is a superset of `provider`: same exports plus an
// additional `import agent;` so a layer can forward downstream. We
// generate them as separate top-level types (`Provider`, `Layer`) so
// the rest of the host can statically distinguish a terminal stage from
// an intermediate one. The `with:` clause on the layer makes it reuse
// the provider's interface types verbatim — every WIT record/variant is
// defined exactly once under `crate::yosh::acp::*`, and a single
// set of `Host` trait impls on `HostState` satisfies both linkers.
//
// Bindgen flips imports/exports from the host's perspective: imported
// interfaces (`client` for both worlds, plus `agent` for `layer`) become
// `Host` traits we implement; exported interfaces (`agent`) become
// callable methods on the wrapper struct.
wasmtime::component::bindgen!({
    path: "../../vendor/wit",
    world: "provider",
    imports: { default: async },
    exports: { default: async },
});

mod layer_bindings {
    // The layer bindgen lives in its own module so its generated
    // `exports` module and `Layer` world wrapper don't collide with
    // the provider's. Interface types are shared via `with:` so every
    // WIT record/variant is still defined exactly once at the crate
    // root, and a single set of `Host` impls on `HostState` satisfies
    // both linkers.
    wasmtime::component::bindgen!({
        path: "../../vendor/wit",
        world: "layer",
        imports: { default: async },
        exports: { default: async },
        with: {
            "yosh:acp/errors": crate::yosh::acp::errors,
            "yosh:acp/content": crate::yosh::acp::content,
            "yosh:acp/init": crate::yosh::acp::init,
            "yosh:acp/sessions": crate::yosh::acp::sessions,
            "yosh:acp/prompts": crate::yosh::acp::prompts,
            "yosh:acp/tools": crate::yosh::acp::tools,
            "yosh:acp/terminals": crate::yosh::acp::terminals,
            "yosh:acp/filesystem": crate::yosh::acp::filesystem,
            "yosh:acp/client": crate::yosh::acp::client,
        },
    });
}

pub use layer_bindings::Layer;
/// `Host` trait for the layer's *imported* `agent` interface — implemented
/// on `HostState` in [`crate::wasm`] to forward downstream.
pub use layer_bindings::yosh::acp::agent as layer_agent;

use crate::wasm::{SessionFactory, SessionRegistry, Stage, StageKind};

#[derive(Parser)]
struct Args {
    /// Path to the terminal ACP **provider** wasm component (the bottom of
    /// the chain). Required.
    ///
    /// Accepts either the legacy positional path (`host my-agent.wasm`)
    /// or the explicit `--provider` flag. The flag takes precedence.
    #[arg(long, value_name = "PATH")]
    provider: Option<PathBuf>,

    /// Legacy positional alias for `--provider`. Retained so existing
    /// invocations keep working unchanged.
    wasm_path: Option<PathBuf>,

    /// Path to a **layer** wasm component to wrap the provider. May be
    /// passed multiple times; layers are applied editor-side → provider-
    /// side in the order given (the first `--layer` is the outermost
    /// stage closest to the host).
    #[arg(long = "layer", value_name = "PATH")]
    layers: Vec<PathBuf>,

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
    init_logging(&args)?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    // Resolve provider path: `--provider` takes precedence, fall back to
    // the legacy positional argument.
    let provider_path = args
        .provider
        .clone()
        .or_else(|| args.wasm_path.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing provider wasm component: pass `--provider <path>` or as positional arg"
            )
        })?;

    let provider = load_stage(&engine, &provider_path, StageKind::Provider)?;
    info!(
        provider = %provider.component_id,
        layer_count = args.layers.len(),
        "chain configuration",
    );

    let layers: Vec<Stage> = args
        .layers
        .iter()
        .map(|p| load_stage(&engine, p, StageKind::Layer))
        .collect::<Result<_>>()?;
    for (idx, stage) in layers.iter().enumerate() {
        info!(idx, layer = %stage.component_id, "loaded layer");
    }

    let data_root = init_data_root()?;

    // Multi-threaded runtime + `LocalSet`: pins `!Send` session actors to
    // the `block_on` thread while `Send` work runs on the worker pool.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let local = LocalSet::new();
    local.block_on(&runtime, async move {
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let factory = Arc::new(SessionFactory::new(
            engine,
            provider,
            layers,
            outbound_tx,
            data_root,
        ));
        let registry = Arc::new(SessionRegistry::new());

        info!(path = %provider_path.display(), "loaded provider component");
        info!("listening for ACP JSON-RPC on stdio");

        bridge::run(factory, registry, outbound_rx).await
    })
}

/// Load a wasm component from disk and pair it with a stable component
/// id derived from the filename. Used for both the provider and each
/// layer stage. Validates the component's import set against the world
/// it was passed as so a layer-shaped wasm passed via `--provider` (or
/// vice versa) is rejected at boot rather than failing later at
/// instantiation with a less obvious error.
fn load_stage(engine: &Engine, path: &std::path::Path, kind: StageKind) -> Result<Stage> {
    let component = Component::from_file(engine, path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading {}", path.display()))?;
    validate_imports(engine, &component, kind)
        .with_context(|| format!("validating {}", path.display()))?;
    let component_id =
        utils::component_id_from_path(path).context("deriving component id from wasm filename")?;
    Ok(Stage {
        component,
        component_id,
    })
}

/// Reject components whose `agent` import status doesn't match the world
/// they were passed as. Imports are versioned (`yosh:acp/agent@…`),
/// so we match on the unversioned prefix to stay forward-compatible with
/// minor WIT bumps.
fn validate_imports(engine: &Engine, component: &Component, kind: StageKind) -> Result<()> {
    let ty = component.component_type();
    let imports_agent = ty
        .imports(engine)
        .any(|(name, _)| name.starts_with("yosh:acp/agent"));
    match (kind, imports_agent) {
        (StageKind::Provider, true) => anyhow::bail!(
            "component imports `yosh:acp/agent` (it is a layer); \
             pass it via `--layer` rather than `--provider`",
        ),
        (StageKind::Layer, false) => anyhow::bail!(
            "component does not import `yosh:acp/agent` (it is a provider); \
             pass it via `--provider` rather than `--layer`",
        ),
        _ => Ok(()),
    }
}

/// Configure the global `tracing` subscriber. Stderr is always wired up;
/// `--log-file` adds an opt-in file layer (ANSI off, so the file stays
/// grep-friendly). Each boot writes to its own timestamped file —
/// e.g. `host.log` becomes `host-<unix-ts>.log` — so runs never
/// stomp each other and old logs stick around for postmortems.
/// `RUST_LOG` takes precedence over the
/// `--log-filter` / `--log-level` flags.
fn init_logging(args: &Args) -> Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let directive = args
            .log_filter
            .clone()
            .unwrap_or_else(|| format!("host={}", args.log_level.as_str()));
        tracing_subscriber::EnvFilter::new(directive)
    });

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let log_path = args.log_file.as_deref().map(timestamped_log_path);
    let file_layer = log_path.as_deref().map(open_log_file).transpose()?;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    if let Some(path) = log_path.as_deref() {
        info!(path = %path.display(), "mirroring logs to file");
    }

    Ok(())
}

/// Insert a unix-seconds timestamp before the extension so each boot
/// gets its own file. `logs/host.log` -> `logs/host-1714838400.log`.
fn timestamped_log_path(path: &std::path::Path) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("host");
    let ext = path.extension().and_then(|s| s.to_str());
    let name = match ext {
        Some(ext) => format!("{stem}-{ts}.{ext}"),
        None => format!("{stem}-{ts}"),
    };
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// Open `path` (creating parent dirs as needed) and wrap it in a non-ANSI
/// `tracing_subscriber` layer suitable for appending logs to.
fn open_log_file<S>(
    path: &std::path::Path,
) -> Result<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening log file {}", path.display()))?;
    // truncate is a no-op on the fresh timestamped path, but keeps
    // behavior sane if the user happens to point at an existing file.

    let subscriber = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(file);

    Ok(Box::new(subscriber))
}

/// Resolve and create the per-app data root, returning its path.
///
/// Each session gets a project- and component-scoped subdirectory
/// underneath this:
///
///   `<data_root>/<project_id>/<component_id>/`    <-- mounted at /data
///
/// `<project_id>` is a hash of the session's cwd (no path leakage in
/// the dir name); `<component_id>` is the wasm filename stem. The
/// result: data is naturally siloed per project so an agent can't
/// accidentally leak history between unrelated codebases.
fn init_data_root() -> Result<PathBuf> {
    let data_root = resolve_data_root().context("resolving data root")?;
    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("creating data root {}", data_root.display()))?;
    info!(path = %data_root.display(), "data root");
    Ok(data_root)
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
