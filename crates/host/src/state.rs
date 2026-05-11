//! Per-store host state and the WASI / wasmtime-side trait impls. The actual
//! `client::Host` impl lives in [`crate::client_impl`].

use std::sync::Arc;

use agent_client_protocol::{Error as AcpError, schema};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView, default_hooks};

use crate::secrets::SecretsRegistry;

/// Events the wasm-side host trait sends out to the bridge task. The bridge
/// task is the only one with the JSON-RPC `ConnectionTo`, so the host trait
/// shoves work onto this channel and (for requests) awaits the reply.
pub enum OutboundEvent {
    /// One-way `session/update` notification.
    SessionUpdate(schema::SessionNotification),
    /// `fs/read_text_file` request that expects a response.
    ReadTextFile(
        schema::ReadTextFileRequest,
        oneshot::Sender<Result<schema::ReadTextFileResponse, AcpError>>,
    ),
    /// `fs/write_text_file` request that expects a response.
    WriteTextFile(
        schema::WriteTextFileRequest,
        oneshot::Sender<Result<schema::WriteTextFileResponse, AcpError>>,
    ),
}

/// Per-store host state. Each wasm instance gets exactly one of these.
pub struct HostState {
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub table: ResourceTable,
    /// Where the wasm-side `client::Host` impl routes outbound client
    /// calls (notifications and request/response). Either straight to
    /// the bridge task ([`ClientSink::Outbound`], for the topmost stage
    /// in the chain) or up into the next layer's exported `client`
    /// interface ([`ClientSink::Upstream`]). See [`ClientSink`] for the
    /// rationale on why upstream forwarding targets a *separate* wasm
    /// instance from the agent direction.
    pub client_sink: ClientSink,
    /// Next stage in the layer chain, if any. Populated for layer
    /// instances; `None` for the terminal provider. The `agent::Host`
    /// impl on `HostState` forwards each imported-`agent` call by sending
    /// a message on this actor's channel.
    pub downstream: Option<DownstreamHandle>,
    /// Component id of this stage, used to scope secret lookups.
    pub component_id: String,
    /// Shared secrets registry. Lookups are scoped by `component_id`.
    pub secrets: Arc<SecretsRegistry>,
}

/// Where the `client::Host` impl forwards outbound client calls.
#[derive(Clone)]
pub enum ClientSink {
    /// Top of the chain: client calls go straight to the bridge task as
    /// `OutboundEvent`s. Bounded send for backpressure.
    Outbound(mpsc::Sender<OutboundEvent>),
    /// Forward into the upstream layer's exported `client` interface.
    /// Held as a `Weak` to avoid a strong-ref cycle with the downstream
    /// pointer (the chain owns each stage strongly via `downstream`; the
    /// upstream sink is a back edge).
    Upstream(UpstreamHandle),
}

/// Weak channel handle to the upstream layer's [`WasmActor`]. Weak so
/// that the back-edge between paired stages doesn't keep either alive
/// after the chain is dropped.
pub type UpstreamHandle = crate::wasm_actor::WasmActorWeak;

/// Strong channel handle to the next stage's [`WasmActor`]. The chain
/// owns each stage strongly via `downstream`; calls become messages on
/// the channel — no mutex, no nested `run_concurrent`.
pub type DownstreamHandle = crate::wasm_actor::WasmActor;

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

impl wasmtime_wasi_http::p3::WasiHttpView for HostState {
    fn http(&mut self) -> wasmtime_wasi_http::p3::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p3::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

impl crate::yosh::acp::errors::Host for HostState {}
impl crate::yosh::acp::sessions::Host for HostState {}
impl crate::yosh::acp::content::Host for HostState {}
impl crate::yosh::acp::terminals::Host for HostState {}
impl crate::yosh::acp::tools::Host for HostState {}
impl crate::yosh::acp::prompts::Host for HostState {}
impl crate::yosh::acp::filesystem::Host for HostState {}
impl crate::yosh::acp::init::Host for HostState {}
