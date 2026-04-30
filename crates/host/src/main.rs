//! ACP wasmtime host.
//!
//! Loads an ACP agent component and bridges it to the editor over the ACP
//! JSON-RPC wire protocol on stdio. Logs go to stderr; configure verbosity
//! with the `RUST_LOG` environment variable (e.g. `RUST_LOG=host=debug`).

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::role::acp::Agent as AgentRole;
use agent_client_protocol::{ByteStreams, schema};
use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, info, warn};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView, default_hooks};

mod translate;

// Generate wasmtime component bindings for the `agent-plugin` world. From the
// host's perspective, bindgen flips imports/exports: the `client` interface
// becomes a Host trait we implement, and the `agent` interface becomes
// callable methods on the bindings struct.
wasmtime::component::bindgen!({
    path: "../../vendor/wit",
    world: "agent-plugin",
    imports: { default: async },
    exports: { default: async },
});

use yoshuawuyts::acp::types as acp;

// -----------------------------------------------------------------------------
// Per-store host state.
// -----------------------------------------------------------------------------

struct HostState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    /// Channel for forwarding wasm-side `update-session` calls to the ACP
    /// connection task. Drained by the `connect_with` main loop.
    updates: mpsc::UnboundedSender<schema::SessionNotification>,
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
// `client` interface implementation. Called by the wasm guest.
// -----------------------------------------------------------------------------

impl yoshuawuyts::acp::client::Host for HostState {
    async fn update_session(&mut self, session_id: acp::SessionId, update: acp::SessionUpdate) {
        if let Some(notif) = translate::session_update_wit_to_schema(session_id, update) {
            // Best-effort: if the receiver is gone, the connection has shut
            // down; nothing useful to do here.
            let _ = self.updates.send(notif);
        }
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

    async fn write_text_file(&mut self, _req: acp::WriteTextFileRequest) -> Result<(), acp::Error> {
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
        Err(translate::method_not_found(
            "release-terminal not supported",
        ))
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
    async fn new(
        engine: &Engine,
        component: &Component,
        updates_tx: mpsc::UnboundedSender<schema::SessionNotification>,
    ) -> Result<Self> {
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
            updates: updates_tx,
        };
        let mut store = Store::new(engine, state);
        let bindings = AgentPlugin::instantiate_async(&mut store, component, &linker).await?;
        Ok(Self { store, bindings })
    }

    // Disjoint-borrow helpers: each method splits `&mut self` into separate
    // mutable refs to `store` and an immutable borrow of `bindings`, so the
    // wasmtime call can take `&mut self.store` while the bindings accessor
    // remains live.

    async fn call_initialize(
        &mut self,
        req: &acp::InitializeRequest,
    ) -> wasmtime::Result<Result<acp::InitializeResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_initialize(&mut self.store, req)
            .await
    }

    async fn call_authenticate(
        &mut self,
        req: &acp::AuthenticateRequest,
    ) -> wasmtime::Result<Result<(), acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_authenticate(&mut self.store, req)
            .await
    }

    async fn call_new_session(
        &mut self,
        req: &acp::NewSessionRequest,
    ) -> wasmtime::Result<Result<acp::NewSessionResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_new_session(&mut self.store, req)
            .await
    }

    async fn call_load_session(
        &mut self,
        req: &acp::LoadSessionRequest,
    ) -> wasmtime::Result<Result<(), acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_load_session(&mut self.store, req)
            .await
    }

    async fn call_prompt(
        &mut self,
        req: &acp::PromptRequest,
    ) -> wasmtime::Result<Result<acp::PromptResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_prompt(&mut self.store, req)
            .await
    }

    async fn call_cancel(&mut self, sid: &acp::SessionId) -> wasmtime::Result<()> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_cancel(&mut self.store, sid)
            .await
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

    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    let agent = Arc::new(Mutex::new(
        WasmAgent::new(&engine, &component, updates_tx).await?,
    ));

    info!(path = %args.wasm_path.display(), "loaded wasm component");
    info!("listening for ACP JSON-RPC on stdio");

    // ACP bridge over stdio. The host plays the `Agent` role on the wire (the
    // editor is the client driving us), so we dispatch incoming agent
    // requests into the wasm component.
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );

    let agent_init = agent.clone();
    let agent_auth = agent.clone();
    let agent_new = agent.clone();
    let agent_load = agent.clone();
    let agent_prompt = agent.clone();
    let agent_cancel = agent.clone();

    AgentRole
        .builder()
        .name("ollama-wasm-host")
        .on_receive_request(
            async move |req: schema::InitializeRequest, responder, _cx| {
                let mut a = agent_init.lock().await;
                let wit_req = translate::init_request_schema_to_wit(req);
                let result = a
                    .call_initialize(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("initialize", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::init_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::AuthenticateRequest, responder, _cx| {
                let mut a = agent_auth.lock().await;
                let wit_req = translate::authenticate_request_schema_to_wit(req);
                let result = a
                    .call_authenticate(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("authenticate", e))?;
                result.map_err(translate::wit_error_to_acp)?;
                let empty: schema::AuthenticateResponse =
                    serde_json::from_value(serde_json::json!({})).expect("empty auth response");
                responder.respond(empty)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::NewSessionRequest, responder, _cx| {
                let mut a = agent_new.lock().await;
                let wit_req = translate::new_session_request_schema_to_wit(req);
                let result = a
                    .call_new_session(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("new-session", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::new_session_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::LoadSessionRequest, responder, _cx| {
                let mut a = agent_load.lock().await;
                let wit_req = translate::load_session_request_schema_to_wit(req);
                let result = a
                    .call_load_session(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("load-session", e))?;
                result.map_err(translate::wit_error_to_acp)?;
                let empty: schema::LoadSessionResponse =
                    serde_json::from_value(serde_json::json!({})).expect("empty load response");
                responder.respond(empty)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::PromptRequest, responder, _cx| {
                debug!(session = %req.session_id.0, "session/prompt");
                let mut a = agent_prompt.lock().await;
                let wit_req = translate::prompt_request_schema_to_wit(req);
                let result = a
                    .call_prompt(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("prompt", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::prompt_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::CancelNotification, _cx| {
                let mut a = agent_cancel.lock().await;
                let sid = translate::cancel_session_id_schema_to_wit(&notif);
                a.call_cancel(&sid).await.ok();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            // Drain wasm-emitted session updates and forward as JSON-RPC
            // notifications to the client (editor) until the channel closes.
            while let Some(notif) = updates_rx.recv().await {
                if let Err(e) = cx.send_notification(notif) {
                    warn!("failed to send session/update: {e:?}");
                    break;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("acp connection error: {e:?}"))?;

    Ok(())
}
