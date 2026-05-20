//! Wasm chain lifecycle and per-session ownership.
//!
//! Every ACP session owns one [`Session`]: a single
//! [`Store<HostState>`] hosting *all* chain stages (the provider plus
//! any layers). A [`tokio::sync::Mutex`] around the store serialises
//! top-level entry points per session — matching the previous
//! actor-based ordering — while concurrency *within* a single
//! `run_concurrent` call is provided by wasmtime's async component
//! model.
//!
//! Calls into the chain head are direct:
//!
//! ```ignore
//! store.lock().await.run_concurrent(async |a| {
//!     a.with(|x| x.get().push_stage(head_idx));
//!     let res = head_bindings.yosh_acp_agent().call_X(a, req).await;
//!     a.with(|x| x.get().pop_stage());
//!     res
//! }).await
//! ```
//!
//! Layers' `agent` imports forward downstream by directly invoking the
//! next stage's bindings on the same store (no mpsc, no oneshot — see
//! [`layer_agent::HostWithStore`] below). Same for client-direction
//! upstream forwarding (see [`crate::client_impl`]).
//!
//! Stateless calls (`initialize`, `authenticate`) build a throwaway
//! `Session` via [`SessionFactory::instantiate`], use it once, and drop
//! it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_concurrency::future::Race;
use tokio::sync::{mpsc, watch};
use wasmtime::component::{Component, HasSelf, Linker, ResourceAny, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::secrets::SecretsRegistry;
use crate::state::{Bindings, ClientSink, HostState, OutboundEvent, StageData, StageKind};
use crate::yosh::acp::errors::Error;
use crate::yosh::acp::init::{AuthenticateRequest, InitializeRequest, InitializeResponse};
use crate::yosh::acp::prompts::PromptResponse;
use crate::yosh::acp::sessions::{
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
};
use crate::{Layer, Provider};

// -----------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------

/// One stage in the routing chain: a pre-loaded wasm `Component` plus the
/// stable component id used to scope its `/data` preopen and secret
/// lookups.
#[derive(Clone)]
pub struct Stage {
    pub component: Component,
    pub component_id: String,
}

/// Produces fresh single-store chains on demand. Cheap: instantiation
/// from pre-loaded `Component`s is microseconds per stage.
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

    /// Build a session with no `/data` preopen. Used for stateless calls.
    pub async fn instantiate(&self) -> Result<Session> {
        self.instantiate_chain(None).await
    }

    /// Component id used by the bridge to label session modes.
    pub fn component_id(&self) -> &str {
        &self.provider.component_id
    }

    /// Shared wasmtime [`Engine`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Build a chain with `/data` preopened to a project-scoped subdir
    /// (provider-scoped only — layers do not get distinct preopens).
    pub async fn instantiate_for_project(&self, cwd: &std::path::Path) -> Result<Session> {
        let project_id = project_id_from_cwd(cwd);
        let project_dir = self.data_root.join(&project_id);
        update_project_meta(&project_dir, cwd);
        self.instantiate_chain(Some(&project_dir)).await
    }

    /// Build the chain into a single shared store.
    ///
    /// Wiring (indices into `HostState::stages`):
    /// - `0` = provider (bottom).
    /// - `1..=layers.len()` = layers, bottom-up (`stages.last()` is the
    ///   chain head — the outermost layer or the provider when no
    ///   layers are configured).
    ///
    /// The provider stage's `downstream_idx = None`; each layer's
    /// `downstream_idx = Some(i - 1)`. Sinks: the outermost stage's
    /// sink is `Outbound`; every other stage's sink is
    /// `Upstream(i + 1)` (one step closer to the editor).
    async fn instantiate_chain(&self, project_dir: Option<&std::path::Path>) -> Result<Session> {
        let stage_count = self.layers.len() + 1;
        let head_idx = stage_count - 1;

        // Pre-allocate stage metadata with bindings=None.
        let mut stages: Vec<StageData> = Vec::with_capacity(stage_count);
        // Stage 0: provider.
        stages.push(StageData {
            kind: StageKind::Provider,
            component_id: self.provider.component_id.clone(),
            bindings: None,
            sink: if self.layers.is_empty() {
                ClientSink::Outbound(self.outbound.clone())
            } else {
                ClientSink::Upstream(1)
            },
            downstream_idx: None,
        });
        // Layers, bottom-up.
        for (i, layer) in self.layers.iter().rev().enumerate() {
            let idx = i + 1;
            let sink = if idx == stage_count - 1 {
                ClientSink::Outbound(self.outbound.clone())
            } else {
                ClientSink::Upstream(idx + 1)
            };
            stages.push(StageData {
                kind: StageKind::Layer,
                component_id: layer.component_id.clone(),
                bindings: None,
                sink,
                downstream_idx: Some(idx - 1),
            });
        }

        // Single WasiCtx for the whole session — layers do not want
        // distinct preopens. The `/data` preopen is provider-scoped.
        let provider_data = stage_data_dir(project_dir, &self.provider.component_id)?;
        let mut wasi = WasiCtxBuilder::new();
        wasi.stderr(crate::wasi_log::TracingStream::new("stderr"))
            .stdout(crate::wasi_log::TracingStream::new("stdout"))
            .inherit_network()
            .inherit_env();
        if let Some(dir) = provider_data.as_deref() {
            wasi.preopened_dir(dir, "/data", DirPerms::all(), FilePerms::all())?;
        }

        let state = HostState {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            stages,
            stage_stack: Vec::with_capacity(4),
            secrets: self.secrets.clone(),
            downstream_sessions: std::collections::HashMap::new(),
            next_downstream_rep: 1,
        };
        let mut store = Store::new(&self.engine, state);

        // Shared linker for every stage. Crucially, this gives every
        // component instance the *same* resource type identity for
        // `yosh:acp/agent/session`, so a `ResourceAny` returned from
        // the provider's `new-session` export can be lifted into the
        // layer's imported `session` slot via `try_into_resource`
        // without a "resource type mismatch" trap.
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        // `Layer::add_to_linker` is a superset of `Provider::add_to_linker`
        // (both worlds now import the same set since the provider also
        // imports `agent` for the `session` resource's destructor), so
        // calling it once suffices for either component shape.
        Layer::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

        // Instantiate each stage's component against the shared linker.
        for idx in 0..stage_count {
            let kind = store.data().stages[idx].kind;
            let component = if idx == 0 {
                &self.provider.component
            } else {
                // `stages[1]` is the bottom-most layer (last in
                // `self.layers`, since we iterated `rev()` above).
                &self.layers[self.layers.len() - idx].component
            };
            let bindings = match kind {
                StageKind::Provider => Bindings::Provider(
                    Provider::instantiate_async(&mut store, component, &linker).await?,
                ),
                StageKind::Layer => {
                    Bindings::Layer(Layer::instantiate_async(&mut store, component, &linker).await?)
                }
            };
            store.data_mut().stages[idx].bindings = Some(Arc::new(bindings));
        }

        debug_assert_chain_wiring(store.data());

        let (cancel_tx, _cancel_rx) = watch::channel(false);
        Ok(Session::new(store, head_idx, cancel_tx))
    }
}

/// Sanity-check the chain wiring after `instantiate_chain` populates
/// stages. Catches indexing typos at build time instead of as confusing
/// test failures later.
fn debug_assert_chain_wiring(state: &HostState) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut outbound_count = 0;
    for (i, s) in state.stages.iter().enumerate() {
        assert!(s.bindings.is_some(), "stage {i} bindings missing");
        match &s.sink {
            ClientSink::Outbound(_) => outbound_count += 1,
            ClientSink::Upstream(parent) => assert!(
                *parent < state.stages.len(),
                "stage {i} upstream points out of range"
            ),
        }
        match s.kind {
            StageKind::Provider => assert!(s.downstream_idx.is_none()),
            StageKind::Layer => {
                let d = s.downstream_idx.expect("layer downstream_idx");
                assert!(d < state.stages.len());
            }
        }
    }
    assert_eq!(
        outbound_count, 1,
        "exactly one stage must have Outbound sink"
    );
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
// Session — direct-linked chain
// -----------------------------------------------------------------------------

/// Outcome of a `prompt` call (matches the previous actor's API so the
/// bridge handler is unchanged).
pub enum PromptOutcome {
    Done(PromptResponse),
    Cancelled,
    Wit(Error),
    Trap(wasmtime::Error),
}

pub enum SetModeOutcome {
    Done,
    Wit(Error),
    Trap(wasmtime::Error),
}

/// One ACP session: a shared store hosting every chain stage, plus the
/// head index and a cancel watch. Cheap to clone (it's an `Arc`).
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    /// `tokio::sync::Mutex` so top-level entry points serialise per
    /// session without blocking the runtime worker thread.
    store: tokio::sync::Mutex<Store<HostState>>,
    head_idx: usize,
    cancel: watch::Sender<bool>,
    /// Owned chain-head `session` resource handle, populated after
    /// `new-session` / `load-session` / `resume-session`. Held inside
    /// the `Store`, so it (and any downstream resources transitively
    /// owned by guest-side wrappers like `LayerSession.downstream`)
    /// are cleaned up when this `SessionInner` drops: `Store::drop`
    /// walks the resource table firing every wasm-side destructor.
    head_session: Mutex<Option<ResourceAny>>,
}

impl Session {
    fn new(store: Store<HostState>, head_idx: usize, cancel: watch::Sender<bool>) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                store: tokio::sync::Mutex::new(store),
                head_idx,
                cancel,
                head_session: Mutex::new(None),
            }),
        }
    }

    pub fn cancel(&self) {
        let _ = self.inner.cancel.send(true);
    }

    /// Run `body` inside `store.run_concurrent`, pushing/popping the
    /// chain-head stage idx around it.
    async fn run_head<F, R>(&self, body: F) -> wasmtime::Result<R>
    where
        F: for<'a> FnOnce(
                &'a wasmtime::component::Accessor<HostState, HasSelf<HostState>>,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>>
            + Send,
        R: Send,
    {
        let head_idx = self.inner.head_idx;
        let mut store = self.inner.store.lock().await;
        store
            .run_concurrent(async move |a| {
                a.with(|mut x| x.get().push_stage(head_idx));
                let r = body(a).await;
                a.with(|mut x| x.get().pop_stage());
                r
            })
            .await
    }

    pub async fn call_initialize(
        &self,
        req: InitializeRequest,
    ) -> wasmtime::Result<Result<InitializeResponse, Error>> {
        let head_idx = self.inner.head_idx;
        self.run_head(|a| {
            Box::pin(async move {
                let bindings = a
                    .with(|mut x| x.get().stages[head_idx].bindings.clone())
                    .expect("head bindings filled");
                match &*bindings {
                    Bindings::Provider(b) => b.yosh_acp_agent().call_initialize(a, req).await,
                    Bindings::Layer(b) => b.yosh_acp_agent().call_initialize(a, req).await,
                }
            })
        })
        .await?
    }

    pub async fn call_authenticate(
        &self,
        req: AuthenticateRequest,
    ) -> wasmtime::Result<Result<(), Error>> {
        let head_idx = self.inner.head_idx;
        self.run_head(|a| {
            Box::pin(async move {
                let bindings = a
                    .with(|mut x| x.get().stages[head_idx].bindings.clone())
                    .expect("head bindings filled");
                match &*bindings {
                    Bindings::Provider(b) => b.yosh_acp_agent().call_authenticate(a, req).await,
                    Bindings::Layer(b) => b.yosh_acp_agent().call_authenticate(a, req).await,
                }
            })
        })
        .await?
    }

    pub async fn call_new_session(
        &self,
        req: NewSessionRequest,
    ) -> wasmtime::Result<Result<NewSessionResponse, Error>> {
        let head_idx = self.inner.head_idx;
        let inner = self.inner.clone();
        self.run_head(|a| {
            Box::pin(async move {
                let bindings = a
                    .with(|mut x| x.get().stages[head_idx].bindings.clone())
                    .expect("head bindings filled");
                let raw = match &*bindings {
                    Bindings::Provider(b) => b.yosh_acp_agent().call_new_session(a, req).await,
                    Bindings::Layer(b) => b.yosh_acp_agent().call_new_session(a, req).await,
                };
                raw.map(|r| {
                    r.map(|(resource, resp)| {
                        *inner.head_session.lock().unwrap() = Some(resource);
                        resp
                    })
                })
            })
        })
        .await?
    }

    pub async fn call_load_session(
        &self,
        req: LoadSessionRequest,
    ) -> wasmtime::Result<Result<LoadSessionResponse, Error>> {
        let head_idx = self.inner.head_idx;
        let inner = self.inner.clone();
        self.run_head(|a| {
            Box::pin(async move {
                let bindings = a
                    .with(|mut x| x.get().stages[head_idx].bindings.clone())
                    .expect("head bindings filled");
                let raw = match &*bindings {
                    Bindings::Provider(b) => b.yosh_acp_agent().call_load_session(a, req).await,
                    Bindings::Layer(b) => b.yosh_acp_agent().call_load_session(a, req).await,
                };
                raw.map(|r| {
                    r.map(|(resource, resp)| {
                        *inner.head_session.lock().unwrap() = Some(resource);
                        resp
                    })
                })
            })
        })
        .await?
    }

    pub async fn set_mode(
        &self,
        mode_id: crate::yosh::acp::sessions::SessionModeId,
    ) -> SetModeOutcome {
        let head_idx = self.inner.head_idx;
        let head_session = match *self.inner.head_session.lock().unwrap() {
            Some(any) => any,
            None => {
                return SetModeOutcome::Trap(wasmtime::Error::msg(
                    "set-mode called before new-session",
                ));
            }
        };
        let res = self
            .run_head(|a| {
                Box::pin(async move {
                    let bindings = a
                        .with(|mut x| x.get().stages[head_idx].bindings.clone())
                        .expect("head bindings filled");
                    match &*bindings {
                        Bindings::Provider(b) => {
                            b.yosh_acp_agent()
                                .session()
                                .call_set_mode(a, head_session, mode_id)
                                .await
                        }
                        Bindings::Layer(b) => {
                            b.yosh_acp_agent()
                                .session()
                                .call_set_mode(a, head_session, mode_id)
                                .await
                        }
                    }
                })
            })
            .await;
        match res {
            Err(e) => SetModeOutcome::Trap(e),
            Ok(Err(e)) => SetModeOutcome::Trap(e),
            Ok(Ok(Err(e))) => SetModeOutcome::Wit(e),
            Ok(Ok(Ok(()))) => SetModeOutcome::Done,
        }
    }

    pub async fn select_model(
        &self,
        model_id: crate::yosh::acp::sessions::SessionModelId,
    ) -> SetModeOutcome {
        let head_idx = self.inner.head_idx;
        let head_session = match *self.inner.head_session.lock().unwrap() {
            Some(any) => any,
            None => {
                return SetModeOutcome::Trap(wasmtime::Error::msg(
                    "select-model called before new-session",
                ));
            }
        };
        let res = self
            .run_head(|a| {
                Box::pin(async move {
                    let bindings = a
                        .with(|mut x| x.get().stages[head_idx].bindings.clone())
                        .expect("head bindings filled");
                    match &*bindings {
                        Bindings::Provider(b) => {
                            b.yosh_acp_agent()
                                .session()
                                .call_select_model(a, head_session, model_id)
                                .await
                        }
                        Bindings::Layer(b) => {
                            b.yosh_acp_agent()
                                .session()
                                .call_select_model(a, head_session, model_id)
                                .await
                        }
                    }
                })
            })
            .await;
        match res {
            Err(e) => SetModeOutcome::Trap(e),
            Ok(Err(e)) => SetModeOutcome::Trap(e),
            Ok(Ok(Err(e))) => SetModeOutcome::Wit(e),
            Ok(Ok(Ok(()))) => SetModeOutcome::Done,
        }
    }

    /// Race the chain-head `prompt` call against the cancel watch.
    /// Dropping the prompt future on cancel releases the store lock and
    /// wasmtime cancels any in-flight component tasks.
    ///
    /// Phase 3 v1: gets the prompt-turn resource from
    /// `session.prompt()`, immediately closes its `updates()` stream
    /// reader (so the guest's `emit_update` writes resolve to
    /// `StreamResult::Dropped` instead of hanging), and awaits
    /// `response()`. The body stream is dropped on the floor; phase 3
    /// v2 implements a `StreamConsumer` that pushes each update onto
    /// the outbound bridge as a `session/update` notification.
    pub async fn prompt(
        &self,
        prompt: Vec<crate::yosh::acp::content::ContentBlock>,
    ) -> PromptOutcome {
        let _ = self.inner.cancel.send_replace(false);
        let mut cancel_rx = self.inner.cancel.subscribe();
        cancel_rx.mark_unchanged();

        let head_idx = self.inner.head_idx;
        let head_session = match *self.inner.head_session.lock().unwrap() {
            Some(any) => any,
            None => {
                return PromptOutcome::Trap(wasmtime::Error::msg(
                    "prompt called before new-session",
                ));
            }
        };
        let this = self.clone();
        let prompt_arm = async move {
            let res = this
                .run_head(|a| {
                    Box::pin(async move {
                        let bindings = a
                            .with(|mut x| x.get().stages[head_idx].bindings.clone())
                            .expect("head bindings filled");
                        // Construct the prompt-turn resource.
                        let turn = match &*bindings {
                            Bindings::Provider(b) => {
                                b.yosh_acp_agent()
                                    .session()
                                    .call_prompt(a, head_session, prompt)
                                    .await?
                            }
                            Bindings::Layer(b) => {
                                b.yosh_acp_agent()
                                    .session()
                                    .call_prompt(a, head_session, prompt)
                                    .await?
                            }
                        };
                        // Drain the updates body. v1: close the reader
                        // immediately so guest writes resolve to
                        // `StreamResult::Dropped` rather than blocking
                        // on backpressure with no reader. v2 wires a
                        // real `StreamConsumer` that forwards each
                        // update to the outbound bridge as a
                        // `session/update` notification.
                        let mut reader = match &*bindings {
                            Bindings::Provider(b) => {
                                b.yosh_acp_agent()
                                    .prompt_turn()
                                    .call_updates(a, turn)
                                    .await?
                            }
                            Bindings::Layer(b) => {
                                b.yosh_acp_agent()
                                    .prompt_turn()
                                    .call_updates(a, turn)
                                    .await?
                            }
                        };
                        // Close the reader. Errors here are reported
                        // but not fatal; if the reader's already
                        // closed (e.g. the guest dropped it), that's
                        // a no-op.
                        if let Err(e) = reader.close_with(a) {
                            tracing::debug!(error = %e, "close updates reader");
                        }
                        // Await the final response.
                        let resp = match &*bindings {
                            Bindings::Provider(b) => {
                                b.yosh_acp_agent()
                                    .prompt_turn()
                                    .call_response(a, turn)
                                    .await
                            }
                            Bindings::Layer(b) => {
                                b.yosh_acp_agent()
                                    .prompt_turn()
                                    .call_response(a, turn)
                                    .await
                            }
                        };
                        // Drop the resource. Phase 3 v1 skips the
                        // explicit drop — the resource lives until
                        // the store tears down at session end. Phase
                        // 5 wires `resource_drop_async` properly so
                        // turns are recycled mid-session.
                        let _ = turn;
                        Ok::<_, wasmtime::Error>(resp)
                    })
                })
                .await;
            match res {
                Err(e) => PromptOutcome::Trap(e),
                Ok(Err(e)) => PromptOutcome::Trap(e),
                Ok(Ok(Err(e))) => PromptOutcome::Trap(e),
                Ok(Ok(Ok(Err(e)))) => PromptOutcome::Wit(e),
                Ok(Ok(Ok(Ok(r)))) => PromptOutcome::Done(r),
            }
        };
        let cancel_arm = async move {
            let _ = cancel_rx.changed().await;
            PromptOutcome::Cancelled
        };
        (cancel_arm, prompt_arm).race().await
    }
}

// -----------------------------------------------------------------------------
// Registry
// -----------------------------------------------------------------------------

pub struct SessionRegistry {
    // KNOWN LIMITATION: entries are inserted by `session/new` and
    // `session/load` handlers but never removed — the upstream ACP
    // protocol at this version has no `session/close` notification
    // from the editor, and we removed our WIT `close-session` in
    // favor of the now-resource-based lifecycle. As a result the map
    // grows monotonically for the lifetime of the host process.
    // Each entry holds an `Arc<SessionInner>` (one `Store`, a few
    // `tokio::sync` primitives) which is bounded but not negligible
    // for hosts churning many sessions. Real fix: either route a
    // host-side timeout / explicit `/close` command through here, or
    // wait for ACP to add an editor-driven close signal.
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert(&self, id: String, session: Session) {
        self.lock().insert(id, session);
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.lock().get(id).cloned()
    }

    #[allow(dead_code)]
    pub fn remove(&self, id: &str) -> Option<Session> {
        self.lock().remove(id)
    }
}

// -----------------------------------------------------------------------------
// agent::Host (layer's imported `agent`) — forwards to downstream stage
// -----------------------------------------------------------------------------
//
// A layer's `agent` import lands on this trait. The impl reads the
// currently executing stage's `downstream_idx` and calls the downstream
// stage's exported `agent` directly on the shared store — no mpsc, no
// oneshot. Push/pop the stage stack around the call so the linker's
// host getter returns the correct stage for any nested host imports.

use crate::layer_agent;
use crate::translate;
use crate::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, ResumeSessionRequest, ResumeSessionResponse,
};

/// Stash a downstream `ResourceAny` (returned from the next stage's
/// exported `agent.session`) in `HostState` and mint a fresh typed
/// `Resource<layer_agent::Session>` whose `rep` is the stash key.
///
/// Cross-instance resource transfer trips wasmtime's resource
/// type-identity check — even with a shared linker, the provider's
/// export-side `Session` type identity differs from the layer's
/// import-side `Session`. The indirection here keeps the downstream
/// resource alive (held by the host) while presenting the layer with
/// a freshly-typed handle owned by its own instance.
fn stash_layer_session(
    accessor: &wasmtime::component::Accessor<impl Send, HasSelf<HostState>>,
    any: ResourceAny,
) -> wasmtime::component::Resource<layer_agent::Session> {
    let rep = accessor.with(|mut a| a.get().stash_downstream_session(any));
    wasmtime::component::Resource::new_own(rep)
}

fn no_downstream<T>(method: &'static str) -> Result<T, Error> {
    Err(no_downstream_err(method))
}

fn no_downstream_err(method: &'static str) -> Error {
    translate::internal_error(&format!(
        "layer called `agent.{method}` but no downstream is configured"
    ))
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

/// Resolve the downstream stage's bindings + idx for a layer's import.
/// Returns `None` if there's no downstream (host bug — providers don't
/// import `agent`).
fn downstream(
    accessor: &wasmtime::component::Accessor<impl Send, HasSelf<HostState>>,
) -> Option<(usize, Arc<Bindings>)> {
    accessor.with(|mut a| {
        let state = a.get();
        let stage = state.current_stage();
        let idx = stage.downstream_idx?;
        let bindings = state.stages[idx]
            .bindings
            .clone()
            .expect("downstream bindings filled");
        Some((idx, bindings))
    })
}

async fn downstream_call<R, F, Fut>(
    accessor: &wasmtime::component::Accessor<impl Send, HasSelf<HostState>>,
    idx: usize,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    accessor.with(|mut a| a.get().push_stage(idx));
    let r = f().await;
    accessor.with(|mut a| a.get().pop_stage());
    r
}

impl layer_agent::Host for HostState {}

/// Host-side `drop` for the layer-imported `agent.session` resource.
///
/// Removes the bookkeeping entry from [`HostState::downstream_sessions`].
/// The stashed `ResourceAny` is then Rust-dropped; the downstream stage's
/// wasm-side `Drop for ProviderSession`/`PlanSession` fires when the
/// containing `Store` tears down (i.e. when the host's [`Session`] drops).
/// In-flight cleanup mid-session isn't possible from here because the
/// trait method has no `Accessor` to invoke `ResourceAny::resource_drop_async`
/// on, but the per-session window matches the chain's lifetime anyway:
/// the layer's own `Session` resource (and its `downstream` field) only
/// drop when the chain head drops.
impl layer_agent::HostSession for HostState {
    async fn drop(
        &mut self,
        rep: wasmtime::component::Resource<layer_agent::Session>,
    ) -> wasmtime::Result<()> {
        let _ = self.take_downstream_session(rep.rep());
        Ok(())
    }
}

impl layer_agent::HostPromptTurn for HostState {
    async fn drop(
        &mut self,
        _rep: wasmtime::component::Resource<layer_agent::PromptTurn>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl layer_agent::HostPromptTurnWithStore for HasSelf<HostState> {
    fn updates<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<layer_agent::PromptTurn>,
    ) -> impl ::core::future::Future<
        Output = wasmtime::component::StreamReader<crate::yosh::acp::prompts::SessionUpdate>,
    > + Send {
        async move {
            // Phase 3 stub: empty stream.
            accessor
                .with(|mut a| {
                    wasmtime::component::StreamReader::new(
                        &mut a,
                        std::iter::empty::<crate::yosh::acp::prompts::SessionUpdate>(),
                    )
                })
                .expect("empty stream construction")
        }
    }

    fn response<T: Send>(
        _accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<layer_agent::PromptTurn>,
    ) -> impl ::core::future::Future<Output = Result<PromptResponse, Error>> + Send {
        async move {
            Err(translate::internal_error(
                "phase 3: layer_agent::HostPromptTurnWithStore::response",
            ))
        }
    }
}

/// Host-owned payload for a `client.terminal` resource.
///
/// Phase 2: holds the create-terminal-request that constructed it. A
/// real implementation would also carry editor-side correlation state
/// (a wire-level `terminal-id` returned from the editor's
/// `terminal/create` JSON-RPC, plus the process state). For now the
/// host doesn't actually start a process — `output()` returns an empty
/// stream and `wait_for_exit()` returns an error.
pub struct HostTerminalEntry {
    pub _req: crate::yosh::acp::terminals::CreateTerminalRequest,
}

impl crate::yosh::acp::client::HostTerminal for HostState {
    async fn new(
        &mut self,
        req: crate::yosh::acp::terminals::CreateTerminalRequest,
    ) -> wasmtime::component::Resource<crate::yosh::acp::client::Terminal> {
        // Allocate a slot in the per-store resource table. The rep we
        // mint here is the table-side rep, retagged under the WIT
        // `Terminal` resource type (same pattern as
        // [`crate::secrets_impl::StoreHost::get`]).
        let entry = self
            .table
            .push(HostTerminalEntry { _req: req })
            .expect("resource table push for client.terminal");
        wasmtime::component::Resource::new_own(entry.rep())
    }

    async fn drop(
        &mut self,
        rep: wasmtime::component::Resource<crate::yosh::acp::client::Terminal>,
    ) -> wasmtime::Result<()> {
        let entry: wasmtime::component::Resource<HostTerminalEntry> =
            wasmtime::component::Resource::new_own(rep.rep());
        let _ = self.table.delete(entry);
        Ok(())
    }
}

impl crate::yosh::acp::client::HostTerminalWithStore for HasSelf<HostState> {
    fn output<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<crate::yosh::acp::client::Terminal>,
    ) -> impl ::core::future::Future<Output = wasmtime::component::StreamReader<u8>> + Send {
        async move {
            // Phase 2 placeholder: an immediately-finished empty stream.
            // Phase 5 wires this to `terminal/output` polling on the
            // outbound bridge, with the writer pumping bytes from editor
            // responses until exit.
            accessor
                .with(|mut a| {
                    wasmtime::component::StreamReader::new(&mut a, std::iter::empty::<u8>())
                })
                .expect("empty stream construction")
        }
    }

    fn wait_for_exit<T: Send>(
        _accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<crate::yosh::acp::client::Terminal>,
    ) -> impl ::core::future::Future<
        Output = Result<crate::yosh::acp::terminals::TerminalExitStatus, Error>,
    > + Send {
        async move {
            Err(translate::internal_error(
                "client.terminal.wait_for_exit not yet implemented; \
                 no process is spawned by the host stub",
            ))
        }
    }
}

impl crate::yosh::acp::tools::HostToolCall for HostState {
    async fn new(
        &mut self,
        _initial: crate::yosh::acp::tools::ToolCallInit,
    ) -> wasmtime::component::Resource<crate::yosh::acp::tools::ToolCall> {
        unimplemented!("phase 4: tools::HostToolCall::new")
    }

    async fn drop(
        &mut self,
        _rep: wasmtime::component::Resource<crate::yosh::acp::tools::ToolCall>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl crate::yosh::acp::tools::HostToolCallWithStore for HasSelf<HostState> {
    fn update<T: Send>(
        _accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<crate::yosh::acp::tools::ToolCall>,
        _patch: crate::yosh::acp::tools::ToolCallPatch,
    ) -> impl ::core::future::Future<Output = ()> + Send {
        async move { unimplemented!("phase 4: tools::HostToolCallWithStore::update") }
    }
}

impl layer_agent::HostWithStore for HasSelf<HostState> {
    fn initialize<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: InitializeRequest,
    ) -> impl ::core::future::Future<Output = Result<InitializeResponse, Error>> + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return no_downstream("initialize");
            };
            let res = downstream_call(accessor, idx, || async {
                match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_initialize(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_initialize(accessor, req).await,
                }
            })
            .await;
            flatten_downstream("initialize", res)
        }
    }

    fn authenticate<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: AuthenticateRequest,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return no_downstream("authenticate");
            };
            let res = downstream_call(accessor, idx, || async {
                match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_authenticate(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_authenticate(accessor, req).await,
                }
            })
            .await;
            flatten_downstream("authenticate", res)
        }
    }

    fn new_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: NewSessionRequest,
    ) -> impl ::core::future::Future<
        Output = Result<
            (
                wasmtime::component::Resource<layer_agent::Session>,
                NewSessionResponse,
            ),
            Error,
        >,
    > + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return Err(no_downstream_err("new-session"));
            };
            let res: wasmtime::Result<Result<(ResourceAny, NewSessionResponse), Error>> =
                downstream_call(accessor, idx, || async {
                    match &*bindings {
                        Bindings::Provider(b) => {
                            b.yosh_acp_agent().call_new_session(accessor, req).await
                        }
                        Bindings::Layer(b) => {
                            b.yosh_acp_agent().call_new_session(accessor, req).await
                        }
                    }
                })
                .await;
            let (any, resp) = flatten_downstream("new-session", res)?;
            let resource = stash_layer_session(accessor, any);
            Ok((resource, resp))
        }
    }

    fn load_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: LoadSessionRequest,
    ) -> impl ::core::future::Future<
        Output = Result<
            (
                wasmtime::component::Resource<layer_agent::Session>,
                LoadSessionResponse,
            ),
            Error,
        >,
    > + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return Err(no_downstream_err("load-session"));
            };
            let res: wasmtime::Result<Result<(ResourceAny, LoadSessionResponse), Error>> =
                downstream_call(accessor, idx, || async {
                    match &*bindings {
                        Bindings::Provider(b) => {
                            b.yosh_acp_agent().call_load_session(accessor, req).await
                        }
                        Bindings::Layer(b) => {
                            b.yosh_acp_agent().call_load_session(accessor, req).await
                        }
                    }
                })
                .await;
            let (any, resp) = flatten_downstream("load-session", res)?;
            let resource = stash_layer_session(accessor, any);
            Ok((resource, resp))
        }
    }

    fn list_sessions<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: ListSessionsRequest,
    ) -> impl ::core::future::Future<Output = Result<ListSessionsResponse, Error>> + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return no_downstream("list-sessions");
            };
            let res = downstream_call(accessor, idx, || async {
                match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_list_sessions(accessor, req).await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent().call_list_sessions(accessor, req).await
                    }
                }
            })
            .await;
            flatten_downstream("list-sessions", res)
        }
    }

    fn resume_session<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        req: ResumeSessionRequest,
    ) -> impl ::core::future::Future<
        Output = Result<
            (
                wasmtime::component::Resource<layer_agent::Session>,
                ResumeSessionResponse,
            ),
            Error,
        >,
    > + Send {
        let ds = downstream(accessor);
        async move {
            let Some((idx, bindings)) = ds else {
                return Err(no_downstream_err("resume-session"));
            };
            let res: wasmtime::Result<Result<(ResourceAny, ResumeSessionResponse), Error>> =
                downstream_call(accessor, idx, || async {
                    match &*bindings {
                        Bindings::Provider(b) => {
                            b.yosh_acp_agent().call_resume_session(accessor, req).await
                        }
                        Bindings::Layer(b) => {
                            b.yosh_acp_agent().call_resume_session(accessor, req).await
                        }
                    }
                })
                .await;
            let (any, resp) = flatten_downstream("resume-session", res)?;
            let resource = stash_layer_session(accessor, any);
            Ok((resource, resp))
        }
    }
}

/// Host-side glue for the layer-imported `agent.session` resource.
/// `set-mode` and `select-model` forward to the downstream stashed
/// resource; `prompt` returns a (phase-3 stub) prompt-turn resource
/// that the host wires when streams are real.
impl layer_agent::HostSessionWithStore for HasSelf<HostState> {
    fn prompt<T: Send>(
        _accessor: &wasmtime::component::Accessor<T, Self>,
        _self_: wasmtime::component::Resource<layer_agent::Session>,
        _prompt: Vec<crate::yosh::acp::content::ContentBlock>,
    ) -> impl ::core::future::Future<
        Output = wasmtime::component::Resource<layer_agent::PromptTurn>,
    > + Send {
        async move {
            unimplemented!(
                "phase 3: layer_agent::HostSessionWithStore::prompt forwards downstream prompt-turn"
            )
        }
    }

    fn set_mode<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: wasmtime::component::Resource<layer_agent::Session>,
        mode_id: crate::yosh::acp::sessions::SessionModeId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let downstream = accessor.with(|mut a| {
            let state = a.get();
            let stage = state.current_stage();
            let idx = stage.downstream_idx?;
            let bindings = state.stages[idx].bindings.clone()?;
            let any = state.downstream_sessions.get(&self_.rep()).copied()?;
            Some((idx, bindings, any))
        });
        async move {
            let Some((idx, bindings, any)) = downstream else {
                return Err(translate::internal_error(
                    "layer called `session.set-mode` but no downstream session is mapped",
                ));
            };
            let res = downstream_call(accessor, idx, || async {
                match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent()
                            .session()
                            .call_set_mode(accessor, any, mode_id)
                            .await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent()
                            .session()
                            .call_set_mode(accessor, any, mode_id)
                            .await
                    }
                }
            })
            .await;
            flatten_downstream("session.set-mode", res)
        }
    }

    fn select_model<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        self_: wasmtime::component::Resource<layer_agent::Session>,
        model_id: crate::yosh::acp::sessions::SessionModelId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let downstream = accessor.with(|mut a| {
            let state = a.get();
            let stage = state.current_stage();
            let idx = stage.downstream_idx?;
            let bindings = state.stages[idx].bindings.clone()?;
            let any = state.downstream_sessions.get(&self_.rep()).copied()?;
            Some((idx, bindings, any))
        });
        async move {
            let Some((idx, bindings, any)) = downstream else {
                return Err(translate::internal_error(
                    "layer called `session.select-model` but no downstream session is mapped",
                ));
            };
            let res = downstream_call(accessor, idx, || async {
                match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent()
                            .session()
                            .call_select_model(accessor, any, model_id)
                            .await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent()
                            .session()
                            .call_select_model(accessor, any, model_id)
                            .await
                    }
                }
            })
            .await;
            flatten_downstream("session.select-model", res)
        }
    }
}
