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
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::WasiHttpCtx;

use crate::Provider;
use crate::acp;
use crate::state::{HostState, OutboundEvent};

// -----------------------------------------------------------------------------
// Factory
// -----------------------------------------------------------------------------

/// Produces fresh wasm instances on demand. Cheap: instantiation from a
/// pre-loaded `Component` is microseconds.
pub struct SessionFactory {
    engine: Engine,
    component: Component,
    outbound: mpsc::Sender<OutboundEvent>,
}

impl SessionFactory {
    pub fn new(
        engine: Engine,
        component: Component,
        outbound: mpsc::Sender<OutboundEvent>,
    ) -> Self {
        Self {
            engine,
            component,
            outbound,
        }
    }

    /// Build a fresh wasm instance with its own store and `HostState`. All
    /// instances share the same outbound channel, so the bridge task can
    /// drain events from any session through one receiver.
    pub async fn instantiate(&self) -> Result<WasmAgent> {
        WasmAgent::new(&self.engine, &self.component, self.outbound.clone()).await
    }
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
    Done(acp::PromptResponse),
    Cancelled,
    Wit(acp::Error),
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
        req: acp::PromptRequest,
        reply: oneshot::Sender<PromptOutcome>,
    },
}

/// Bridge-side handle to a [`SessionActor`]. Cloneable, `Send + Sync`.
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<Message>,
    /// Out-of-band cancel signal. The actor races each prompt against this
    /// via `tokio::select!`, so cancel bypasses the message queue. Putting
    /// cancel on the queue would defeat the purpose: it would wait behind
    /// the very prompt it's supposed to interrupt.
    cancel: watch::Sender<bool>,
}

impl SessionHandle {
    pub async fn prompt(&self, req: acp::PromptRequest) -> Result<PromptOutcome, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::Prompt { req, reply: tx })
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
                    let outcome = tokio::select! {
                        biased;
                        _ = self.cancel.changed() => PromptOutcome::Cancelled,
                        r = self.agent.call_prompt(&req) => match r {
                            Err(e) => PromptOutcome::Trap(e),
                            Ok(Err(e)) => PromptOutcome::Wit(e),
                            Ok(Ok(resp)) => PromptOutcome::Done(resp),
                        }
                    };
                    if reply.send(outcome).is_err() {
                        warn!("prompt caller dropped before response was sent");
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
    ) -> Result<Self> {
        let mut linker: Linker<HostState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        Provider::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)?;

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
        let bindings = Provider::instantiate_async(&mut store, component, &linker).await?;
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
    ) -> wasmtime::Result<Result<acp::LoadSessionResponse, acp::Error>> {
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
}
