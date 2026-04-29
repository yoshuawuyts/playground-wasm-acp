//! ACP wasmtime host.
//!
//! Loads an ACP agent component and bridges it to the editor over the ACP
//! JSON-RPC wire protocol on stdio.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::Mutex;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView, default_hooks};

mod translate;

// Generate wasmtime component bindings for the `agent-plugin` world (the world
// the wasm guest implements). From the host's perspective:
// - the `client` interface is imported by the guest, so we must provide a
//   `Host` trait impl on our state type.
// - the `agent` interface is exported by the guest, so we get callable
//   functions on the generated bindings struct.
wasmtime::component::bindgen!({
    path: "../../vendor/wit",
    world: "agent-plugin",
    imports: { default: async },
    exports: { default: async },
});

use yoshuawuyts::acp::types as acp;

/// Per-store host state.
struct HostState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: default_hooks(),
        }
    }
}

impl yoshuawuyts::acp::types::Host for HostState {}

// -----------------------------------------------------------------------------
// `client` interface implementation. These methods are called by the wasm
// guest. For the MVP they log/forward what we can and return method-not-found
// for the rest.
// -----------------------------------------------------------------------------

impl yoshuawuyts::acp::client::Host for HostState {
    async fn update_session(
        &mut self,
        session_id: acp::SessionId,
        update: acp::SessionUpdate,
    ) {
        // TODO: forward to the editor via the ACP connection. For now, log to
        // stderr so we can see it during smoke tests.
        eprintln!(
            "[host] update-session {session_id}: {}",
            translate::session_update_summary(&update)
        );
    }

    async fn request_permission(
        &mut self,
        _req: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        Err(translate::method_not_found("request-permission not wired"))
    }

    async fn read_text_file(
        &mut self,
        _req: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(translate::method_not_found("read-text-file not supported"))
    }

    async fn write_text_file(
        &mut self,
        _req: acp::WriteTextFileRequest,
    ) -> Result<(), acp::Error> {
        Err(translate::method_not_found("write-text-file not supported"))
    }

    async fn create_terminal(
        &mut self,
        _req: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(translate::method_not_found("create-terminal not supported"))
    }

    async fn get_terminal_output(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<acp::TerminalOutput, acp::Error> {
        Err(translate::method_not_found(
            "get-terminal-output not supported",
        ))
    }

    async fn wait_for_terminal_exit(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<acp::TerminalExitStatus, acp::Error> {
        Err(translate::method_not_found(
            "wait-for-terminal-exit not supported",
        ))
    }

    async fn kill_terminal(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<(), acp::Error> {
        Err(translate::method_not_found("kill-terminal not supported"))
    }

    async fn release_terminal(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<(), acp::Error> {
        Err(translate::method_not_found("release-terminal not supported"))
    }
}

// -----------------------------------------------------------------------------
// Wasm runtime wrapper.
// -----------------------------------------------------------------------------

/// Owns the wasmtime store + the instantiated `agent-plugin` bindings.
struct WasmAgent {
    store: Store<HostState>,
    bindings: AgentPlugin,
}

impl WasmAgent {
    async fn new(engine: &Engine, component: &Component) -> Result<Self> {
        let mut linker: Linker<HostState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        AgentPlugin::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

        let state = HostState {
            wasi: WasiCtxBuilder::new()
                .inherit_stderr()
                .inherit_stdout()
                .inherit_network()
                .build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        };
        let mut store = Store::new(engine, state);
        let bindings = AgentPlugin::instantiate_async(&mut store, component, &linker).await?;
        Ok(Self { store, bindings })
    }
}

// -----------------------------------------------------------------------------
// CLI / entry point.
// -----------------------------------------------------------------------------

#[derive(Parser)]
struct Args {
    /// Path to the ACP agent wasm component.
    wasm_path: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &args.wasm_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading {}", args.wasm_path.display()))?;

    let agent = Arc::new(Mutex::new(WasmAgent::new(&engine, &component).await?));

    // TODO Phase 3 finish: wire up agent-client-protocol Builder over stdio,
    // dispatching incoming ACP agent requests (initialize/new-session/prompt/
    // etc.) to `agent.bindings.yoshuawuyts_acp_agent()` calls and translating
    // the schema types via the `translate` module.
    eprintln!("[host] loaded {}", args.wasm_path.display());
    eprintln!("[host] ACP wire protocol bridge not yet wired up");
    let _ = agent;
    bail!("ACP wire protocol bridge not yet implemented")
}

