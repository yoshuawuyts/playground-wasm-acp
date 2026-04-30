//! Wrapper around the wasmtime store and instantiated `agent-plugin`
//! bindings. Exposes thin helpers that disjoint-borrow `&mut self.store` and
//! `&self.bindings` so each call site doesn't have to.

use anyhow::Result;
use tokio::sync::mpsc;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::WasiHttpCtx;

use crate::AgentPlugin;
use crate::state::{HostState, OutboundEvent};
use crate::yoshuawuyts::acp::types as acp;

/// Owns the wasmtime store + the instantiated `agent-plugin` bindings.
pub struct WasmAgent {
    store: Store<HostState>,
    bindings: AgentPlugin,
}

impl WasmAgent {
    pub async fn new(
        engine: &Engine,
        component: &Component,
        outbound: mpsc::Sender<OutboundEvent>,
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
            outbound,
        };
        let mut store = Store::new(engine, state);
        let bindings = AgentPlugin::instantiate_async(&mut store, component, &linker).await?;
        Ok(Self { store, bindings })
    }

    pub async fn call_initialize(
        &mut self,
        req: &acp::InitializeRequest,
    ) -> wasmtime::Result<Result<acp::InitializeResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_initialize(&mut self.store, req)
            .await
    }

    pub async fn call_authenticate(
        &mut self,
        req: &acp::AuthenticateRequest,
    ) -> wasmtime::Result<Result<(), acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_authenticate(&mut self.store, req)
            .await
    }

    pub async fn call_new_session(
        &mut self,
        req: &acp::NewSessionRequest,
    ) -> wasmtime::Result<Result<acp::NewSessionResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_new_session(&mut self.store, req)
            .await
    }

    pub async fn call_load_session(
        &mut self,
        req: &acp::LoadSessionRequest,
    ) -> wasmtime::Result<Result<(), acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_load_session(&mut self.store, req)
            .await
    }

    pub async fn call_prompt(
        &mut self,
        req: &acp::PromptRequest,
    ) -> wasmtime::Result<Result<acp::PromptResponse, acp::Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_prompt(&mut self.store, req)
            .await
    }

    pub async fn call_cancel(&mut self, sid: &acp::SessionId) -> wasmtime::Result<()> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_cancel(&mut self.store, sid)
            .await
    }
}
