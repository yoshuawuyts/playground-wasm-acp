//! Per-store host state and the WASI / wasmtime-side trait impls. The actual
//! `client::Host` impl lives in [`crate::client_impl`].

use agent_client_protocol::schema;
use tokio::sync::mpsc;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView, default_hooks};

/// Per-store host state. Each wasm instance gets exactly one of these.
pub struct HostState {
    pub wasi: WasiCtx,
    pub http: WasiHttpCtx,
    pub table: ResourceTable,
    /// Channel for forwarding wasm-side `update-session` calls to the ACP
    /// connection task. Drained by the `connect_with` main loop.
    pub updates: mpsc::UnboundedSender<schema::SessionNotification>,
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
