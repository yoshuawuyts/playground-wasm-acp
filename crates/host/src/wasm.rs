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

use crate::Provider;
use crate::state::{HostState, OutboundEvent};
use crate::yoshuawuyts::acp::errors::Error;
use crate::yoshuawuyts::acp::init::{AuthenticateRequest, InitializeRequest, InitializeResponse};
use crate::yoshuawuyts::acp::prompts::{PromptRequest, PromptResponse};
use crate::yoshuawuyts::acp::sessions::{
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    SetSessionModeRequest,
};

// -----------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------

/// Produces fresh wasm instances on demand. Cheap: instantiation from a
/// pre-loaded `Component` is microseconds.
///
/// The factory owns the data *root* and the component id. Per-session data
/// dirs are constructed at instantiation time as
/// `<data_root>/<project_id>/<component_id>/`, where `project_id` is a
/// deterministic hash of the session's working directory. This keeps state
/// siloed per project so an agent can't accidentally leak data between
/// codebases.
///
/// Stateless calls (`initialize`, `authenticate`) bypass `/data` entirely
/// via [`Self::instantiate`]; session-creating calls use
/// [`Self::instantiate_for_project`].
pub struct SessionFactory {
    engine: Engine,
    component: Component,
    outbound: mpsc::Sender<OutboundEvent>,
    data_root: PathBuf,
    component_id: String,
}

impl SessionFactory {
    pub fn new(
        engine: Engine,
        component: Component,
        outbound: mpsc::Sender<OutboundEvent>,
        data_root: PathBuf,
        component_id: String,
    ) -> Self {
        Self {
            engine,
            component,
            outbound,
            data_root,
            component_id,
        }
    }

    /// Build a wasm instance with no `/data` preopen. Used for stateless
    /// calls like `initialize` and `authenticate` where there is no
    /// session and therefore no project scope.
    pub async fn instantiate(&self) -> Result<WasmAgent> {
        WasmAgent::new(&self.engine, &self.component, self.outbound.clone(), None).await
    }

    /// The configured component id (typically the wasm filename stem).
    /// Used by the bridge to label session modes with a registry-style
    /// `namespace:name` prefix.
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// Build a wasm instance with `/data` preopened to a project- and
    /// component-scoped subdirectory of the data root. Creates the
    /// directory if missing and updates the project's `meta.json` sidecar.
    pub async fn instantiate_for_project(&self, cwd: &std::path::Path) -> Result<WasmAgent> {
        let project_id = project_id_from_cwd(cwd);
        let project_dir = self.data_root.join(&project_id);
        let component_dir = project_dir.join(&self.component_id);
        std::fs::create_dir_all(&component_dir)
            .with_context(|| format!("creating project data dir {}", component_dir.display()))?;
        // The sidecar lives in `<data_root>/<project_id>/meta.json`, one
        // level above the wasm preopen. Wasm cannot escape upward, so the
        // sidecar is structurally read-only from the guest's perspective.
        update_project_meta(&project_dir, cwd);
        WasmAgent::new(
            &self.engine,
            &self.component,
            self.outbound.clone(),
            Some(&component_dir),
        )
        .await
    }
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

/// Owns the wasmtime store + the instantiated `provider` bindings.
pub struct WasmAgent {
    store: Store<HostState>,
    bindings: Provider,
}

impl WasmAgent {
    pub async fn new(
        engine: &Engine,
        component: &Component,
        outbound: mpsc::Sender<OutboundEvent>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        let mut linker: Linker<HostState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        Provider::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

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
        };
        let mut store = Store::new(engine, state);
        let bindings = Provider::instantiate_async(&mut store, component, &linker).await?;
        Ok(Self { store, bindings })
    }

    pub async fn call_initialize(
        &mut self,
        req: &InitializeRequest,
    ) -> wasmtime::Result<Result<InitializeResponse, Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_initialize(&mut self.store, req)
            .await
    }

    pub async fn call_authenticate(
        &mut self,
        req: &AuthenticateRequest,
    ) -> wasmtime::Result<Result<(), Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_authenticate(&mut self.store, req)
            .await
    }

    pub async fn call_new_session(
        &mut self,
        req: &NewSessionRequest,
    ) -> wasmtime::Result<Result<NewSessionResponse, Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_new_session(&mut self.store, req)
            .await
    }

    pub async fn call_load_session(
        &mut self,
        req: &LoadSessionRequest,
    ) -> wasmtime::Result<Result<LoadSessionResponse, Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_load_session(&mut self.store, req)
            .await
    }

    pub async fn call_set_session_mode(
        &mut self,
        req: &SetSessionModeRequest,
    ) -> wasmtime::Result<Result<(), Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_set_session_mode(&mut self.store, req)
            .await
    }

    pub async fn call_prompt(
        &mut self,
        req: &PromptRequest,
    ) -> wasmtime::Result<Result<PromptResponse, Error>> {
        self.bindings
            .yoshuawuyts_acp_agent()
            .call_prompt(&mut self.store, req)
            .await
    }
}
