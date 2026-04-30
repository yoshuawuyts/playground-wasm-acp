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

impl crate::yoshuawuyts::acp::types::Host for HostState {}
