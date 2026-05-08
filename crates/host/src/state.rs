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
    /// stage's exported `agent`. `Arc<tokio::sync::Mutex<_>>` because
    /// wasmtime's bindgen-generated async traits require `Send` futures.
    pub downstream: Option<DownstreamHandle>,
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

/// Shared handle to an upstream layer's wasm instance. `Weak` to avoid a
/// strong-cycle with the downstream pointer (the chain owns each stage
/// strongly via `downstream`; the upstream pointer is logically a back
/// edge, so a weak reference is correct and prevents leaks at chain
/// teardown).
pub type UpstreamHandle = std::sync::Weak<tokio::sync::Mutex<crate::wasm::WasmAgent>>;

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
