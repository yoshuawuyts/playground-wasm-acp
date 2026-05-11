//! Wasm instance lifecycle and per-session actors.
//!
//! Each ACP session is owned by a [`SessionActor`] — a `!Send` task hosted
//! on the top-level `LocalSet` (see [`crate::main`]). The actor owns its
//! [`ChainHandle`] outright; no shared mutable state. The bridge talks to
//! it through a [`SessionHandle`].
//!
//! A chain is a vector of [`crate::wasm_actor::WasmActor`]s — one per
//! stage. Each actor runs a persistent `Store::run_concurrent` event loop
//! that pulls [`crate::wasm_actor::Cmd`]s off its channel and dispatches
//! them via `accessor.spawn` so calls execute concurrently inside one
//! store. The chain head is the outermost stage (the one the bridge
//! talks to); each stage's host state holds a strong handle to the next
//! stage and a weak handle to the previous one.
//!
//! Stateless calls (`initialize`, `authenticate`) bypass the registry:
//! the bridge spins up a throwaway chain via [`SessionFactory`], uses it
//! once, and drops it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_concurrency::future::Race;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::warn;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::secrets::SecretsRegistry;
use crate::state::{ClientSink, DownstreamHandle, HostState, OutboundEvent};
use crate::wasm_actor::{Bindings, WasmActor};
use crate::yosh::acp::errors::Error;
use crate::yosh::acp::init::{AuthenticateRequest, InitializeRequest, InitializeResponse};
use crate::yosh::acp::prompts::{PromptRequest, PromptResponse};
use crate::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SetSessionModeRequest,
};
use crate::{Layer, Provider};

// -----------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------

/// One stage in the routing chain: a pre-loaded wasm `Component` plus the
/// component id used to scope its `/data` preopen.
#[derive(Clone)]
pub struct Stage {
    pub component: Component,
    pub component_id: String,
}

/// Chain of [`WasmActor`]s. The head is the outermost stage; the join
/// handles are the per-stage actor tasks. Dropping the `ChainHandle`
/// closes every stage's command channel (no other senders remain), which
/// causes each actor loop to exit and the join handles to complete.
pub struct ChainHandle {
    pub head: WasmActor,
    /// Kept for supervision; the actor tasks are detached otherwise.
    /// Aborted when the chain is dropped (each loop also exits cleanly
    /// when its channel closes).
    _joins: Vec<JoinHandle<wasmtime::Result<()>>>,
}

impl Drop for ChainHandle {
    fn drop(&mut self) {
        // The channels close once `head` (and the per-stage HostStates,
        // which hold the back-references) drop, so the loops exit on
        // their own. Aborting is a belt-and-braces guard against a
        // wedged store.
        for j in &self._joins {
            j.abort();
        }
    }
}

/// Produces fresh wasm chains on demand. Cheap: instantiation from
/// pre-loaded `Component`s is microseconds per stage.
pub struct SessionFactory {
    engine: Engine,
    /// Terminal provider stage. Always the bottom of the chain.
    provider: Stage,
    /// Layer stages, ordered editor-side → provider-side. Empty means no
    /// layers (legacy single-component behaviour).
    layers: Vec<Stage>,
    outbound: mpsc::Sender<OutboundEvent>,
    data_root: PathBuf,
    secrets: Arc<SecretsRegistry>,
}

impl SessionFactory {
    pub fn new(
        engine: Engine,
        provider: Stage,
        layers: Vec<Stage>,
        outbound: mpsc::Sender<OutboundEvent>,
        data_root: PathBuf,
        secrets: Arc<SecretsRegistry>,
    ) -> Self {
        Self {
            engine,
            provider,
            layers,
            outbound,
            data_root,
            secrets,
        }
    }

    /// Build a chain with no `/data` preopen. Used for stateless calls.
    pub async fn instantiate(&self) -> Result<ChainHandle> {
        self.instantiate_chain(None).await
    }

    fn outbound_sink(&self) -> ClientSink {
        ClientSink::Outbound(self.outbound.clone())
    }

    /// Component id used by the bridge to label session modes.
    pub fn component_id(&self) -> &str {
        &self.provider.component_id
    }

    /// Build a chain with `/data` preopened to a project-scoped subdir.
    pub async fn instantiate_for_project(&self, cwd: &std::path::Path) -> Result<ChainHandle> {
        let project_id = project_id_from_cwd(cwd);
        let project_dir = self.data_root.join(&project_id);
        update_project_meta(&project_dir, cwd);
        self.instantiate_chain(Some(&project_dir)).await
    }

    /// Build the chain bottom-up:
    ///
    /// 1. Allocate a [`WasmActor`] channel pair per stage so every stage
    ///    knows its own and its neighbours' handles before any wasm code
    ///    runs.
    /// 2. Build each stage's `Store` + `HostState` with the correct
    ///    neighbour handles wired in from the start (downstream =
    ///    strong, upstream = weak).
    /// 3. Spawn each stage's actor loop on the tokio runtime.
    ///
    /// The outermost layer's `client_sink` stays as `Outbound` (events
    /// flow to the bridge); inner stages' sinks are `Upstream(weak)` so
    /// outbound client calls re-enter the upper layer's exported
    /// `client` interface.
    async fn instantiate_chain(
        &self,
        project_dir: Option<&std::path::Path>,
    ) -> Result<ChainHandle> {
        // Stage 0: provider (bottom). Stage `1..=layers.len()`: layers,
        // listed bottom-up. So `actors[0]` is the provider, `actors.last()`
        // is the chain head.
        let mut actors: Vec<WasmActor> = Vec::with_capacity(self.layers.len() + 1);
        let mut receivers: Vec<mpsc::Receiver<crate::wasm_actor::Cmd>> =
            Vec::with_capacity(self.layers.len() + 1);
        for _ in 0..self.layers.len() + 1 {
            let (a, rx) = WasmActor::channel();
            actors.push(a);
            receivers.push(rx);
        }

        let mut joins: Vec<JoinHandle<wasmtime::Result<()>>> =
            Vec::with_capacity(self.layers.len() + 1);

        // Build provider stage.
        {
            let provider_data = stage_data_dir(project_dir, &self.provider.component_id)?;
            // If there are layers, the provider's upstream is the
            // first layer (`actors[1]`); otherwise it talks straight
            // to the bridge.
            let client_sink = if self.layers.is_empty() {
                self.outbound_sink()
            } else {
                ClientSink::Upstream(actors[1].downgrade())
            };
            let (store, bindings) = build_stage(
                &self.engine,
                &self.provider.component,
                StageKind::Provider,
                client_sink,
                provider_data.as_deref(),
                None,
                &self.provider.component_id,
                self.secrets.clone(),
            )
            .await?;
            let rx = receivers.remove(0); // own provider rx
            joins.push(WasmActor::spawn_loop(store, bindings, rx));
        }

        // Build each layer, bottom-up. Layers are stored editor-side →
        // provider-side, so `self.layers.last()` sits directly above the
        // provider. We iterate in reverse so we visit innermost first
        // (stage_idx = 1), matching the actor index layout above.
        for (i, stage) in self.layers.iter().rev().enumerate() {
            let stage_idx = i + 1; // 1..=layers.len()
            let data = stage_data_dir(project_dir, &stage.component_id)?;
            let downstream: DownstreamHandle = actors[stage_idx - 1].clone();
            // The outermost layer is the last visited (stage_idx ==
            // layers.len()) and routes client events outward; every
            // other layer routes them up into the next layer.
            let client_sink = if stage_idx == self.layers.len() {
                self.outbound_sink()
            } else {
                ClientSink::Upstream(actors[stage_idx + 1].downgrade())
            };
            let (store, bindings) = build_stage(
                &self.engine,
                &stage.component,
                StageKind::Layer,
                client_sink,
                data.as_deref(),
                Some(downstream),
                &stage.component_id,
                self.secrets.clone(),
            )
            .await?;
            // We pop the front of `receivers` each iteration; provider
            // already removed its own. After the loop `receivers` is
            // empty.
            let rx = receivers.remove(0);
            joins.push(WasmActor::spawn_loop(store, bindings, rx));
        }

        let head = actors.pop().expect("at least one stage");
        Ok(ChainHandle {
            head,
            _joins: joins,
        })
    }
}

/// Build a wasm component instance for one stage: configure WASI,
/// instantiate against the appropriate world's linker, and return the
/// store + bindings ready to be handed to a [`WasmActor`] loop.
#[allow(clippy::too_many_arguments)]
async fn build_stage(
    engine: &Engine,
    component: &Component,
    kind: StageKind,
    client_sink: ClientSink,
    data_dir: Option<&std::path::Path>,
    downstream: Option<DownstreamHandle>,
    component_id: &str,
    secrets: Arc<SecretsRegistry>,
) -> Result<(Store<HostState>, Bindings)> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;

    let mut wasi = WasiCtxBuilder::new();
    wasi.stderr(crate::wasi_log::TracingStream::new("stderr"))
        .stdout(crate::wasi_log::TracingStream::new("stdout"))
        .inherit_network();
    if let Some(dir) = data_dir {
        wasi.preopened_dir(dir, "/data", DirPerms::all(), FilePerms::all())?;
    }
    let state = HostState {
        wasi: wasi.build(),
        http: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        client_sink,
        downstream,
        component_id: component_id.to_string(),
        secrets,
    };
    let mut store = Store::new(engine, state);

    let bindings = match kind {
        StageKind::Provider => {
            Provider::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
            Bindings::Provider(Provider::instantiate_async(&mut store, component, &linker).await?)
        }
        StageKind::Layer => {
            Layer::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
            Bindings::Layer(Layer::instantiate_async(&mut store, component, &linker).await?)
        }
    };
    Ok((store, bindings))
}

/// Which world to instantiate a stage as.
#[derive(Copy, Clone, Debug)]
pub enum StageKind {
    Provider,
    Layer,
}

/// Compute `<project_dir>/<component_id>/` (creating the directory) when a
/// project dir is supplied; otherwise return `None`.
fn stage_data_dir(
    project_dir: Option<&std::path::Path>,
    component_id: &str,
) -> Result<Option<PathBuf>> {
    let Some(project_dir) = project_dir else {
        return Ok(None);
    };
    let dir = project_dir.join(component_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating project data dir {}", dir.display()))?;
    Ok(Some(dir))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ProjectMeta {
    cwd: String,
    first_seen: Option<String>,
    last_used: Option<String>,
}

fn update_project_meta(project_dir: &std::path::Path, cwd: &std::path::Path) {
    let meta_path = project_dir.join("meta.json");
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| format!("{}", d.as_secs()));
    let mut meta: ProjectMeta = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if meta.first_seen.is_none() {
        meta.first_seen = now.clone();
    }
    meta.last_used = now;
    meta.cwd = canon.to_string_lossy().into_owned();
    if let Ok(bytes) = serde_json::to_vec_pretty(&meta) {
        if let Err(e) = std::fs::write(&meta_path, bytes) {
            tracing::debug!(path = %meta_path.display(), error = %e, "failed to write project meta");
        }
    }
}

fn project_id_from_cwd(cwd: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// -----------------------------------------------------------------------------
// Registry
// -----------------------------------------------------------------------------

pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionHandle>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert(&self, id: String, handle: SessionHandle) {
        self.lock().insert(id, handle);
    }

    pub fn get(&self, id: &str) -> Option<SessionHandle> {
        self.lock().get(id).cloned()
    }

    #[allow(dead_code)]
    pub fn remove(&self, id: &str) -> Option<SessionHandle> {
        self.lock().remove(id)
    }
}

// -----------------------------------------------------------------------------
// Session actor
// -----------------------------------------------------------------------------

pub enum PromptOutcome {
    Done(PromptResponse),
    Cancelled,
    Wit(Error),
    Trap(wasmtime::Error),
}

#[derive(Debug)]
pub enum SessionError {
    ChannelClosed,
}

enum Message {
    Prompt {
        req: PromptRequest,
        reply: oneshot::Sender<PromptOutcome>,
    },
    SetMode {
        req: SetSessionModeRequest,
        reply: oneshot::Sender<SetModeOutcome>,
    },
}

pub enum SetModeOutcome {
    Done,
    Wit(Error),
    Trap(wasmtime::Error),
}

#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<Message>,
    cancel: watch::Sender<bool>,
}

impl SessionHandle {
    pub async fn prompt(&self, req: PromptRequest) -> Result<PromptOutcome, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::Prompt { req, reply: tx })
            .await
            .map_err(|_| SessionError::ChannelClosed)?;
        rx.await.map_err(|_| SessionError::ChannelClosed)
    }

    pub async fn set_mode(
        &self,
        req: SetSessionModeRequest,
    ) -> Result<SetModeOutcome, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::SetMode { req, reply: tx })
            .await
            .map_err(|_| SessionError::ChannelClosed)?;
        rx.await.map_err(|_| SessionError::ChannelClosed)
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

/// Per-session actor. Owns the [`ChainHandle`] (so the chain stays alive
/// for the session's lifetime) and serializes prompts / set-mode messages
/// onto its single wasm chain head.
pub struct SessionActor {
    rx: mpsc::Receiver<Message>,
    cancel: watch::Receiver<bool>,
    chain: ChainHandle,
    #[allow(dead_code)]
    peers: Arc<SessionRegistry>,
}

impl SessionActor {
    pub fn new(
        chain: ChainHandle,
        capacity: usize,
        peers: Arc<SessionRegistry>,
    ) -> (Self, SessionHandle) {
        let (tx, rx) = mpsc::channel(capacity);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        (
            Self {
                rx,
                cancel: cancel_rx,
                chain,
                peers,
            },
            SessionHandle {
                tx,
                cancel: cancel_tx,
            },
        )
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                Message::Prompt { req, reply } => {
                    self.cancel.mark_unchanged();
                    let head = self.chain.head.clone();
                    let prompt_arm = async {
                        match head.call_prompt(req).await {
                            Err(e) => PromptOutcome::Trap(e),
                            Ok(Err(e)) => PromptOutcome::Wit(e),
                            Ok(Ok(resp)) => PromptOutcome::Done(resp),
                        }
                    };
                    let cancel_arm = async {
                        let _ = self.cancel.changed().await;
                        PromptOutcome::Cancelled
                    };
                    let outcome = (cancel_arm, prompt_arm).race().await;
                    if reply.send(outcome).is_err() {
                        warn!("prompt caller dropped before response was sent");
                    }
                }
                Message::SetMode { req, reply } => {
                    let outcome = match self.chain.head.call_set_session_mode(req).await {
                        Err(e) => SetModeOutcome::Trap(e),
                        Ok(Err(e)) => SetModeOutcome::Wit(e),
                        Ok(Ok(())) => SetModeOutcome::Done,
                    };
                    if reply.send(outcome).is_err() {
                        warn!("set-mode caller dropped before response was sent");
                    }
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// agent::Host impl — forwards a layer's imported `agent` to its downstream
// -----------------------------------------------------------------------------
//
// The `layer` world imports the `agent` interface; bindgen turns that into
// a `crate::layer_agent::Host` trait. Each method clones the downstream
// [`WasmActor`] handle out of host state and sends a `Cmd` on its
// channel, awaiting the reply. No locks, no nested `run_concurrent`.

use crate::layer_agent;
use crate::translate;

fn no_downstream<T>(method: &'static str) -> Result<T, Error> {
    Err(translate::internal_error(&format!(
        "layer called `agent.{method}` but no downstream is configured"
    )))
}

fn flatten_downstream<T>(
    method: &'static str,
    res: wasmtime::Result<Result<T, Error>>,
) -> Result<T, Error> {
    match res {
        Ok(inner) => inner,
        Err(trap) => Err(translate::internal_error(&format!(
            "downstream `{method}` trapped: {trap:#}"
        ))),
    }
}

impl layer_agent::Host for HostState {}

impl layer_agent::HostWithStore for wasmtime::component::HasSelf<HostState> {
    fn initialize<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: InitializeRequest,
    ) -> impl ::core::future::Future<Output = Result<InitializeResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("initialize");
            };
            flatten_downstream("initialize", ds.call_initialize(req).await)
        }
    }

    fn authenticate<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: AuthenticateRequest,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("authenticate");
            };
            flatten_downstream("authenticate", ds.call_authenticate(req).await)
        }
    }

    fn new_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: NewSessionRequest,
    ) -> impl ::core::future::Future<Output = Result<NewSessionResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("new-session");
            };
            flatten_downstream("new-session", ds.call_new_session(req).await)
        }
    }

    fn load_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: LoadSessionRequest,
    ) -> impl ::core::future::Future<Output = Result<LoadSessionResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("load-session");
            };
            flatten_downstream("load-session", ds.call_load_session(req).await)
        }
    }

    fn list_sessions<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: ListSessionsRequest,
    ) -> impl ::core::future::Future<Output = Result<ListSessionsResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("list-sessions");
            };
            flatten_downstream("list-sessions", ds.call_list_sessions(req).await)
        }
    }

    fn resume_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: ResumeSessionRequest,
    ) -> impl ::core::future::Future<Output = Result<ResumeSessionResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("resume-session");
            };
            flatten_downstream("resume-session", ds.call_resume_session(req).await)
        }
    }

    fn close_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        session_id: SessionId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("close-session");
            };
            flatten_downstream("close-session", ds.call_close_session(session_id).await)
        }
    }

    fn set_session_mode<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: SetSessionModeRequest,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("set-session-mode");
            };
            flatten_downstream("set-session-mode", ds.call_set_session_mode(req).await)
        }
    }

    fn prompt<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: PromptRequest,
    ) -> impl ::core::future::Future<Output = Result<PromptResponse, Error>> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                return no_downstream("prompt");
            };
            flatten_downstream("prompt", ds.call_prompt(req).await)
        }
    }

    fn cancel<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        session_id: SessionId,
    ) -> impl ::core::future::Future<Output = ()> + Send {
        let ds = accessor.with(|mut a| a.get().downstream.clone());
        async move {
            let Some(ds) = ds else {
                tracing::warn!("layer called `agent.cancel` but no downstream is configured");
                return;
            };
            if let Err(trap) = ds.call_cancel(session_id).await {
                tracing::warn!(error = %trap, "downstream `cancel` trapped");
            }
        }
    }
}
