//! Wasm instance lifecycle and per-session actors.
//!
//! Each ACP session is owned by a [`SessionActor`] — a `!Send` task hosted
//! on the top-level `LocalSet` (see [`crate::main`]). The actor owns its
//! [`WasmAgent`] outright; no mutex, no shared mutable state. The bridge
//! talks to it through a [`SessionHandle`].
//!
//! The `LocalSet` is our reachable, structured task pool: every actor's
//! `run` future has a logical parent (the LocalSet, awaited from `main`),
//! so actors can be supervised and shut down explicitly. See
//! <https://blog.yoshuawuyts.com/replacing-tasks-with-actors> for context.
//!
//! Stateless calls (`initialize`, `authenticate`) bypass the actor system:
//! the bridge spins up a throwaway instance via [`SessionFactory`], uses it
//! once, and drops it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_concurrency::future::Race;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::state::{DownstreamHandle, HostState, OutboundEvent};
use crate::yoshuawuyts::acp::errors::Error;
use crate::yoshuawuyts::acp::init::{AuthenticateRequest, InitializeRequest, InitializeResponse};
use crate::yoshuawuyts::acp::prompts::{PromptRequest, PromptResponse};
use crate::yoshuawuyts::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SetSessionModeRequest,
};
use crate::{Layer, Provider};

// -----------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------

/// One stage in the routing chain: a pre-loaded wasm `Component` plus the
/// component id used to scope its `/data` preopen. The id is also used in
/// session-mode prefixes for the outermost (provider) stage.
#[derive(Clone)]
pub struct Stage {
    pub component: Component,
    pub component_id: String,
}

/// Produces fresh wasm instance *chains* on demand. Cheap: instantiation
/// from pre-loaded `Component`s is microseconds per stage.
///
/// A chain is built from a single terminal **provider** plus zero or more
/// **layers** wrapping it. Layers are listed editor-side → provider-side:
/// `layers[0]` is outermost (the host's head), `layers[last]` sits
/// directly above the provider. With no layers, behaviour is identical to
/// the pre-layer host.
///
/// The factory owns the data *root*. Per-session data dirs are constructed
/// at instantiation time as `<data_root>/<project_id>/<component_id>/`,
/// per stage, where `project_id` is a deterministic hash of the session's
/// working directory. This keeps state siloed per project *and* per stage
/// so layers and providers can persist independently.
///
/// Stateless calls (`initialize`, `authenticate`) bypass `/data` entirely
/// via [`Self::instantiate`]; session-creating calls use
/// [`Self::instantiate_for_project`].
pub struct SessionFactory {
    engine: Engine,
    /// Terminal provider stage. Always the bottom of the chain.
    provider: Stage,
    /// Layer stages, ordered editor-side → provider-side. Empty means no
    /// layers (legacy single-component behaviour).
    layers: Vec<Stage>,
    outbound: mpsc::Sender<OutboundEvent>,
    data_root: PathBuf,
}

impl SessionFactory {
    pub fn new(
        engine: Engine,
        provider: Stage,
        layers: Vec<Stage>,
        outbound: mpsc::Sender<OutboundEvent>,
        data_root: PathBuf,
    ) -> Self {
        Self {
            engine,
            provider,
            layers,
            outbound,
            data_root,
        }
    }

    /// Build a wasm instance chain with no `/data` preopen for any stage.
    /// Used for stateless calls like `initialize` and `authenticate`.
    pub async fn instantiate(&self) -> Result<WasmAgent> {
        self.instantiate_chain(None).await
    }

    /// Component id used by the bridge to label session modes. Reports the
    /// *provider's* id since that's the terminal authority for modes;
    /// layers may rewrite the response, but the namespace stays anchored
    /// to the underlying provider.
    pub fn component_id(&self) -> &str {
        &self.provider.component_id
    }

    /// Build a wasm instance chain with `/data` preopened to a project-
    /// scoped subdirectory of the data root for *each* stage. Each stage
    /// gets its own component-scoped subdir
    /// (`<data_root>/<project_id>/<component_id>/`) and an updated
    /// `meta.json` sidecar.
    pub async fn instantiate_for_project(&self, cwd: &std::path::Path) -> Result<WasmAgent> {
        let project_id = project_id_from_cwd(cwd);
        let project_dir = self.data_root.join(&project_id);
        update_project_meta(&project_dir, cwd);
        self.instantiate_chain(Some(&project_dir)).await
    }

    /// Bottom-up chain build: instantiate the provider first, then wrap it
    /// with each layer (innermost-first). The returned `WasmAgent` is the
    /// chain's *head* — the outermost stage that the bridge calls into.
    async fn instantiate_chain(&self, project_dir: Option<&std::path::Path>) -> Result<WasmAgent> {
        // Innermost: terminal provider, no downstream.
        let provider_data = stage_data_dir(project_dir, &self.provider.component_id)?;
        let provider = WasmAgent::new(
            &self.engine,
            &self.provider.component,
            StageKind::Provider,
            self.outbound.clone(),
            provider_data.as_deref(),
            None,
        )
        .await?;

        // Walk layers innermost-first (last → first). Each layer wraps
        // whatever stage we built so far.
        let mut current = provider;
        for stage in self.layers.iter().rev() {
            let downstream: DownstreamHandle =
                std::sync::Arc::new(tokio::sync::Mutex::new(current));
            let data = stage_data_dir(project_dir, &stage.component_id)?;
            current = WasmAgent::new(
                &self.engine,
                &stage.component,
                StageKind::Layer,
                self.outbound.clone(),
                data.as_deref(),
                Some(downstream),
            )
            .await?;
        }
        Ok(current)
    }
}

/// Compute `<project_dir>/<component_id>/` (creating the directory) when a
/// project dir is supplied; otherwise return `None` for a sandboxed
/// no-`/data` instance.
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

/// Project sidecar: human-readable metadata so an operator inspecting the
/// data root can tell which directory belongs to which project. Not used
/// by the runtime; deleting it is harmless.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ProjectMeta {
    /// Canonicalised cwd (best-effort) the project id was hashed from.
    cwd: String,
    /// RFC3339 timestamp of when this project dir was first created.
    first_seen: Option<String>,
    /// RFC3339 timestamp of the last instantiation against this project.
    last_used: Option<String>,
}

/// Best-effort write/refresh of the project meta sidecar. Failures are
/// logged at `debug!` and otherwise ignored — the sidecar is purely a
/// debugging aid and must never block instantiation.
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

/// Hash a working directory to a stable, opaque project id. We canonicalize
/// best-effort (so symlinked variants of the same path collide on the same
/// id) and fall back to the raw path on canonicalization failure (e.g. the
/// directory doesn't exist yet).
///
/// Not cryptographic — this is only a directory bucket. The hash is
/// deliberately opaque so that listing `<data_root>` doesn't reveal which
/// directories the user has worked in.
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

/// Routing table for active sessions. `std::sync::Mutex` is fine: critical
/// sections are one map operation each, never span an `.await`, and the
/// map holds [`SessionHandle`]s (channel senders) with no invariants to
/// violate. Poisoning is recovered explicitly.
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

    /// Remove and return the handle. Dropping all clones causes the actor's
    /// command channel to close; the actor exits its loop and frees the
    /// wasm store.
    #[allow(dead_code)] // used once `close-session` is wired
    pub fn remove(&self, id: &str) -> Option<SessionHandle> {
        self.lock().remove(id)
    }
}

// -----------------------------------------------------------------------------
// Session actor
// -----------------------------------------------------------------------------

/// Outcome of a `prompt` turn. Translation to ACP wire types lives in the
/// bridge; this is the actor-internal vocabulary.
pub enum PromptOutcome {
    Done(PromptResponse),
    Cancelled,
    Wit(Error),
    Trap(wasmtime::Error),
}

/// Channel-layer error: the actor is gone (panic or graceful shutdown
/// before the command was processed). Distinct from anything the wasm
/// guest returned.
#[derive(Debug)]
pub enum SessionError {
    ChannelClosed,
}

/// Commands the bridge sends to a [`SessionActor`].
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

/// Outcome of a `set-session-mode` call. Mirrors [`PromptOutcome`] but
/// without a `Cancelled` arm — mode switches are not cancellable.
pub enum SetModeOutcome {
    Done,
    Wit(Error),
    Trap(wasmtime::Error),
}

/// Bridge-side handle to a [`SessionActor`]. Cloneable, `Send + Sync`.
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<Message>,
    /// Out-of-band cancel signal. The actor races each prompt against this
    /// via `futures_concurrency::Race`, so cancel bypasses the message queue. Putting
    /// cancel on the queue would defeat the purpose: it would wait behind
    /// the very prompt it's supposed to interrupt.
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

    /// Switch the session's active mode. Routed through the actor so it
    /// runs on the same wasm instance (and serializes with prompts) as
    /// the underlying mutable state.
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

    /// Signal cancellation to the actor's currently-running prompt (if
    /// any). Idempotent.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

/// A per-session actor. Owns its [`WasmAgent`] and processes [`Message`]s
/// off a channel. Spawn it onto the top-level `LocalSet` and store the
/// handle in the registry.
///
/// **Inter-session messaging**: each actor holds an `Arc<SessionRegistry>`,
/// so it can look up *other* sessions' [`SessionHandle`]s and call
/// [`SessionHandle::prompt`] / [`SessionHandle::cancel`] on them. This is
/// how a future router/layer/fanout agent would forward work to peers.
/// Direction-of-fanout is the actor's choice; the registry is just a phone
/// book.
///
/// Note: there is no cycle protection. If session A awaits session B which
/// awaits A, both sit forever. The actor model makes this a logical
/// deadlock rather than a lock, but it's still a hang. Add cycle
/// detection only when a real use case demands it.
pub struct SessionActor {
    rx: mpsc::Receiver<Message>,
    cancel: watch::Receiver<bool>,
    agent: WasmAgent,
    /// Phone book for talking to other sessions. Currently unused inside
    /// the actor body — kept here so future inter-session features can
    /// reach the registry without a refactor.
    #[allow(dead_code)]
    peers: Arc<SessionRegistry>,
}

impl SessionActor {
    /// Construct a new actor and its handle. The actor is not running yet;
    /// the caller must drive [`Self::run`] (typically via `spawn_local`
    /// onto the top-level `LocalSet`).
    pub fn new(
        agent: WasmAgent,
        capacity: usize,
        peers: Arc<SessionRegistry>,
    ) -> (Self, SessionHandle) {
        let (tx, rx) = mpsc::channel(capacity);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        (
            Self {
                rx,
                cancel: cancel_rx,
                agent,
                peers,
            },
            SessionHandle {
                tx,
                cancel: cancel_tx,
            },
        )
    }

    /// Drive the actor to completion. Returns when the [`SessionHandle`]
    /// (and all its clones) is dropped, closing the channel.
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg {
                Message::Prompt { req, reply } => {
                    // Reset the watch's "changed" flag so `.changed()`
                    // only fires for cancel signals arriving *during*
                    // this turn.
                    self.cancel.mark_unchanged();
                    // Race the prompt against an out-of-band cancel
                    // signal. Whichever future resolves first decides
                    // the outcome; the loser is dropped (which for the
                    // prompt means tearing down its in-flight wasm
                    // call). `Race` returns the value of whichever
                    // arm wins, so each arm yields a fully-formed
                    // `PromptOutcome`.
                    let prompt_arm = async {
                        match self.agent.call_prompt(&req).await {
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
                    let outcome = match self.agent.call_set_session_mode(&req).await {
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
// WasmAgent
// -----------------------------------------------------------------------------

/// What kind of bindings a stage was instantiated with.
///
/// `Provider` is the terminal stage: only the `client` interface is
/// imported; nothing downstream. `Layer` additionally imports `agent`,
/// which routes to the next stage via [`HostState::downstream`].
///
/// Both variants expose the same `agent` export and therefore the same
/// `call_*` surface — `WasmAgent` dispatches on the variant. We keep them
/// distinct (rather than always using the layer bindings) so the type
/// system, the linker, and the wasmtime instantiation check all agree on
/// which kind of component each path expects.
enum Bindings {
    Provider(Provider),
    Layer(Layer),
}

/// Owns the wasmtime store + the instantiated world bindings for a single
/// stage in the routing chain.
pub struct WasmAgent {
    store: Store<HostState>,
    bindings: Bindings,
}

/// Which world to instantiate a stage as. The chain factory picks
/// `Provider` for the terminal stage and `Layer` for every intermediate
/// stage so the linker matches what the wasm component actually imports.
#[derive(Copy, Clone, Debug)]
pub enum StageKind {
    Provider,
    Layer,
}

impl WasmAgent {
    pub async fn new(
        engine: &Engine,
        component: &Component,
        kind: StageKind,
        outbound: mpsc::Sender<OutboundEvent>,
        data_dir: Option<&std::path::Path>,
        downstream: Option<DownstreamHandle>,
    ) -> Result<Self> {
        let mut linker: Linker<HostState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;

        let mut wasi = WasiCtxBuilder::new();
        wasi.inherit_stderr().inherit_stdout().inherit_network();
        if let Some(dir) = data_dir {
            // `/data` is the component's private read-write state. File
            // access to the user's workspace deliberately *not* preopened:
            // ACP's `fs/read_text_file` / `fs/write_text_file` go through
            // the editor (sees unsaved buffers, capability-gated). A raw
            // preopen would bypass all of that and grant access to every
            // file under cwd, including dotfiles and secrets.
            wasi.preopened_dir(dir, "/data", DirPerms::all(), FilePerms::all())?;
        }
        let state = HostState {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            outbound,
            downstream,
        };
        let mut store = Store::new(engine, state);

        // Provider linker registers only the `client` host trait; layer
        // additionally registers the imported-`agent` host trait that
        // forwards downstream. Picking the right one per stage keeps the
        // linker minimal and lets wasmtime's instantiation check verify
        // the component's import set matches.
        let bindings = match kind {
            StageKind::Provider => {
                Provider::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
                Bindings::Provider(
                    Provider::instantiate_async(&mut store, component, &linker).await?,
                )
            }
            StageKind::Layer => {
                Layer::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;
                Bindings::Layer(Layer::instantiate_async(&mut store, component, &linker).await?)
            }
        };
        Ok(Self { store, bindings })
    }

    pub async fn call_initialize(
        &mut self,
        req: &InitializeRequest,
    ) -> wasmtime::Result<Result<InitializeResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_initialize(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_initialize(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_authenticate(
        &mut self,
        req: &AuthenticateRequest,
    ) -> wasmtime::Result<Result<(), Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_authenticate(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_authenticate(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_new_session(
        &mut self,
        req: &NewSessionRequest,
    ) -> wasmtime::Result<Result<NewSessionResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_new_session(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_new_session(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_load_session(
        &mut self,
        req: &LoadSessionRequest,
    ) -> wasmtime::Result<Result<LoadSessionResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_load_session(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_load_session(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_set_session_mode(
        &mut self,
        req: &SetSessionModeRequest,
    ) -> wasmtime::Result<Result<(), Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_set_session_mode(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_set_session_mode(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_prompt(
        &mut self,
        req: &PromptRequest,
    ) -> wasmtime::Result<Result<PromptResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_prompt(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_prompt(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_list_sessions(
        &mut self,
        req: &ListSessionsRequest,
    ) -> wasmtime::Result<Result<ListSessionsResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_list_sessions(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_list_sessions(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_resume_session(
        &mut self,
        req: &ResumeSessionRequest,
    ) -> wasmtime::Result<Result<ResumeSessionResponse, Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_resume_session(&mut self.store, req)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_resume_session(&mut self.store, req)
                    .await
            }
        }
    }

    pub async fn call_close_session(
        &mut self,
        session_id: &SessionId,
    ) -> wasmtime::Result<Result<(), Error>> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_close_session(&mut self.store, session_id)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_close_session(&mut self.store, session_id)
                    .await
            }
        }
    }

    pub async fn call_cancel(&mut self, session_id: &SessionId) -> wasmtime::Result<()> {
        match &self.bindings {
            Bindings::Provider(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_cancel(&mut self.store, session_id)
                    .await
            }
            Bindings::Layer(b) => {
                b.yoshuawuyts_acp_agent()
                    .call_cancel(&mut self.store, session_id)
                    .await
            }
        }
    }
}

// -----------------------------------------------------------------------------
// agent::Host impl — forwards a layer's imported `agent` to its downstream
// -----------------------------------------------------------------------------
//
// The `layer` world imports the `agent` interface; bindgen turns that into
// a `crate::layer_agent::Host` trait. For each method we lock the
// downstream stage's `WasmAgent` and call its export. Two failure modes
// are flattened into a single WIT `error` returned to the calling layer:
// (a) no downstream is configured (misconfiguration; should not happen
// because only layer wasm components import `agent`, and they are only
// constructed via the chain factory); (b) the downstream traps. The host
// trait return types do not carry a wasmtime trap channel, so a trap
// becomes an `internal-error` from the layer's point of view rather than
// tearing down the whole chain.

use crate::layer_agent;
use crate::translate;

fn no_downstream<T>(method: &'static str) -> Result<T, Error> {
    Err(translate::internal_error(&format!(
        "layer called `agent.{method}` but no downstream is configured"
    )))
}

/// Collapse `wasmtime::Result<Result<T, Error>>` into `Result<T, Error>`
/// for the host trait return types. A trap becomes a WIT `internal-error`
/// so the calling layer sees a recoverable error instead of being torn
/// down with the rest of the chain.
fn flatten<T>(method: &'static str, res: wasmtime::Result<Result<T, Error>>) -> Result<T, Error> {
    match res {
        Ok(inner) => inner,
        Err(trap) => Err(translate::internal_error(&format!(
            "downstream `{method}` trapped: {trap:#}"
        ))),
    }
}

impl layer_agent::Host for HostState {
    async fn initialize(&mut self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("initialize");
        };
        flatten("initialize", ds.lock().await.call_initialize(&req).await)
    }

    async fn authenticate(&mut self, req: AuthenticateRequest) -> Result<(), Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("authenticate");
        };
        flatten(
            "authenticate",
            ds.lock().await.call_authenticate(&req).await,
        )
    }

    async fn new_session(&mut self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("new-session");
        };
        flatten("new-session", ds.lock().await.call_new_session(&req).await)
    }

    async fn load_session(
        &mut self,
        req: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("load-session");
        };
        flatten(
            "load-session",
            ds.lock().await.call_load_session(&req).await,
        )
    }

    async fn list_sessions(
        &mut self,
        req: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("list-sessions");
        };
        flatten(
            "list-sessions",
            ds.lock().await.call_list_sessions(&req).await,
        )
    }

    async fn resume_session(
        &mut self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("resume-session");
        };
        flatten(
            "resume-session",
            ds.lock().await.call_resume_session(&req).await,
        )
    }

    async fn close_session(&mut self, session_id: SessionId) -> Result<(), Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("close-session");
        };
        flatten(
            "close-session",
            ds.lock().await.call_close_session(&session_id).await,
        )
    }

    async fn set_session_mode(&mut self, req: SetSessionModeRequest) -> Result<(), Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("set-session-mode");
        };
        flatten(
            "set-session-mode",
            ds.lock().await.call_set_session_mode(&req).await,
        )
    }

    async fn prompt(&mut self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let Some(ds) = self.downstream.clone() else {
            return no_downstream("prompt");
        };
        flatten("prompt", ds.lock().await.call_prompt(&req).await)
    }

    async fn cancel(&mut self, session_id: SessionId) {
        let Some(ds) = self.downstream.clone() else {
            // `cancel` returns `()`; nothing to report. Log and move on.
            tracing::warn!("layer called `agent.cancel` but no downstream is configured");
            return;
        };
        if let Err(trap) = ds.lock().await.call_cancel(&session_id).await {
            tracing::warn!(error = %trap, "downstream `cancel` trapped");
        }
    }
}
