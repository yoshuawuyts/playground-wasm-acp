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
    /// impl on `HostState` forwards each imported-`agent` call to this
    /// stage's exported `agent`. `Rc<RefCell<_>>` is fine: the actor task
    /// runs on a `LocalSet` (single-threaded), and downstream calls
    /// borrow a *different* `WasmAgent` than the one currently executing.
    pub downstream: Option<DownstreamHandle>,
}

/// Where the `client::Host` impl forwards outbound client calls.
///
/// Two variants because each layer stage actually owns *two* wasm
/// instances (see [`crate::wasm`] docs): one for the agent direction and
/// one for the client direction. Wasmtime stores are non-reentrant, so
/// we cannot invoke a layer's exported `client` interface while its
/// exported `agent` is still executing on the same store. Splitting them
/// across two stores side-steps the reentrancy entirely.
#[derive(Clone)]
pub enum ClientSink {
    /// Top of the chain: client calls go straight to the bridge task as
    /// `OutboundEvent`s. Bounded send for backpressure.
    Outbound(mpsc::Sender<OutboundEvent>),
    /// Forward into the upstream layer's `client_inst` (its dedicated
    /// client-direction wasm instance). The handle points at a
    /// *different* store from whichever one the calling stage is
    /// running on, so this call doesn't reenter.
    Upstream(UpstreamHandle),
}

/// Shared handle to an upstream layer's client-direction wasm instance.
///
/// `Arc<tokio::sync::Mutex<_>>` mirrors [`DownstreamHandle`]: bindgen's
/// async traits require `Send` futures even though the actor and all
/// stages live on a single-threaded `LocalSet`, and the lock is
/// uncontended in practice.
pub type UpstreamHandle = std::sync::Arc<tokio::sync::Mutex<crate::wasm::WasmAgent>>;

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
