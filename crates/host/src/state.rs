//! Per-store host state and the WASI / wasmtime-side trait impls. The actual
//! `client::Host` impl lives in [`crate::client_impl`].

use agent_client_protocol::{Error as AcpError, schema};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView, default_hooks};

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
    /// Outbound channel to the bridge task. Bounded for backpressure: if the
    /// editor falls behind, the wasm side awaits naturally.
    pub outbound: mpsc::Sender<OutboundEvent>,
    /// Next stage in the layer chain, if any. Populated for layer
    /// instances; `None` for the terminal provider. The `agent::Host`
    /// impl on `HostState` forwards each imported-`agent` call to this
    /// stage's exported `agent`. `Rc<RefCell<_>>` is fine: the actor task
    /// runs on a `LocalSet` (single-threaded), and downstream calls
    /// borrow a *different* `WasmAgent` than the one currently executing.
    pub downstream: Option<DownstreamHandle>,
}

/// Shared handle to the next stage's wasm instance. Defined here (rather
/// than in `wasm.rs`) so `HostState` can hold one without a forward
/// reference cycle in the type definition.
///
/// `Arc<tokio::sync::Mutex<_>>` (rather than `Rc<RefCell<_>>`) because
/// wasmtime's bindgen-generated async traits require `Send` futures, even
/// though every stage in a chain ultimately runs on the same `LocalSet`
/// thread. The mutex is uncontended in practice — only the upstream
/// stage's host trait reaches for the downstream — so the lock is just a
/// `Send` adapter.
pub type DownstreamHandle = std::sync::Arc<tokio::sync::Mutex<crate::wasm::WasmAgent>>;

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

impl crate::yoshuawuyts::acp::errors::Host for HostState {}
impl crate::yoshuawuyts::acp::sessions::Host for HostState {}
impl crate::yoshuawuyts::acp::content::Host for HostState {}
impl crate::yoshuawuyts::acp::terminals::Host for HostState {}
impl crate::yoshuawuyts::acp::tools::Host for HostState {}
impl crate::yoshuawuyts::acp::prompts::Host for HostState {}
impl crate::yoshuawuyts::acp::filesystem::Host for HostState {}
impl crate::yoshuawuyts::acp::init::Host for HostState {}
