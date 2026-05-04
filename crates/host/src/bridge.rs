//! Wires the ACP `agent_client_protocol::Builder` over stdio and dispatches
//! incoming JSON-RPC messages to per-session actors.
//!
//! Each session is owned by a [`SessionActor`] hosted on the top-level
//! `LocalSet`. The bridge looks up the session's [`SessionHandle`] in the
//! [`SessionRegistry`] and sends commands over its channel; the actor owns
//! its [`WasmAgent`] outright. No mutex around the wasm instance.
//!
//! Stateless calls (`initialize`, `authenticate`) bypass the actor system:
//! the bridge spins up a throwaway instance via the [`SessionFactory`],
//! uses it, and drops it.
//!
//! Cancellation: the bridge calls [`SessionHandle::cancel`], which sends on
//! a `watch` channel that the actor's `tokio::select!` is racing against.
//! Cancel does not need to acquire the actor's queue and so doesn't wait
//! behind the very prompt it's interrupting.

use std::sync::Arc;

use agent_client_protocol::role::acp::{Agent as AgentRole, Client};
use agent_client_protocol::{ByteStreams, ConnectionTo, Error as AcpError, Responder, schema};
use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, warn};

use crate::state::OutboundEvent;
use crate::translate;
use crate::wasm::{
    PromptOutcome, SessionActor, SessionFactory, SessionHandle, SessionRegistry, SetModeOutcome,
};

/// Look up a session's handle, or return an ACP `invalid-params` error if
/// the session id is unknown.
fn require_session(registry: &SessionRegistry, id: &str) -> Result<SessionHandle, AcpError> {
    registry.get(id).ok_or_else(|| {
        let mut e = AcpError::invalid_params();
        e.message = format!("unknown session id: {id}");
        e
    })
}

/// Run the ACP bridge to completion. Returns when the editor disconnects
/// (stdin EOF) or an error occurs.
pub async fn run(
    factory: Arc<SessionFactory>,
    registry: Arc<SessionRegistry>,
    mut outbound_rx: mpsc::Receiver<OutboundEvent>,
) -> Result<()> {
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );

    // Clone the Arcs once per handler closure. Cheap (Arc bump) and keeps
    // the closures `'static`. Each closure is a thin shim that forwards to
    // a named handler function below — keeping the builder chain readable.
    let factory_init = factory.clone();
    let factory_auth = factory.clone();
    let factory_new = factory.clone();
    let factory_load = factory.clone();
    let registry_new = registry.clone();
    let registry_load = registry.clone();
    let registry_set_mode = registry.clone();
    let registry_prompt = registry.clone();
    let registry_cancel = registry.clone();

    AgentRole
        .builder()
        .name("ollama-wasm-host")
        .on_receive_request(
            async move |req, responder, _cx| handle_initialize(&factory_init, req, responder).await,
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req, responder, _cx| {
                handle_authenticate(&factory_auth, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req, responder, _cx| {
                handle_new_session(&factory_new, &registry_new, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req, responder, _cx| {
                handle_load_session(&factory_load, &registry_load, req, responder).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req, responder, cx| {
                handle_set_session_mode(&registry_set_mode, req, responder, cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req, responder, cx| handle_prompt(&registry_prompt, req, responder, cx),
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif, _cx| handle_cancel(&registry_cancel, notif).await,
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            run_outbound_drain(cx, &mut outbound_rx).await
        })
        .await
        .map_err(|e| anyhow::anyhow!("acp connection error: {e:?}"))?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Request handlers
// -----------------------------------------------------------------------------

async fn handle_initialize(
    factory: &SessionFactory,
    req: schema::InitializeRequest,
    responder: Responder<schema::InitializeResponse>,
) -> Result<(), AcpError> {
    // Throwaway instance: `initialize` carries no session state.
    let mut agent = factory
        .instantiate()
        .await
        .map_err(|e| translate::anyhow_to_acp("initialize: instantiate", e))?;
    tracing::info!(
        fs_read = req.client_capabilities.fs.read_text_file,
        fs_write = req.client_capabilities.fs.write_text_file,
        terminal = req.client_capabilities.terminal,
        "editor capabilities"
    );
    let wit_req = translate::init_request_schema_to_wit(req);
    let result = agent
        .call_initialize(&wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("initialize", e))?;
    let resp = result.map_err(translate::wit_error_to_acp)?;
    responder.respond(translate::init_response_wit_to_schema(resp))
}

async fn handle_authenticate(
    factory: &SessionFactory,
    req: schema::AuthenticateRequest,
    responder: Responder<schema::AuthenticateResponse>,
) -> Result<(), AcpError> {
    // Throwaway instance: `authenticate` is stateless; the host doesn't
    // carry credentials between calls.
    let mut agent = factory
        .instantiate()
        .await
        .map_err(|e| translate::anyhow_to_acp("authenticate: instantiate", e))?;
    let wit_req = translate::authenticate_request_schema_to_wit(req);
    let result = agent
        .call_authenticate(&wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("authenticate", e))?;
    result.map_err(translate::wit_error_to_acp)?;
    responder.respond(translate::empty_authenticate_response()?)
}

async fn handle_new_session(
    factory: &SessionFactory,
    registry: &Arc<SessionRegistry>,
    req: schema::NewSessionRequest,
    responder: Responder<schema::NewSessionResponse>,
) -> Result<(), AcpError> {
    // Spin up a fresh instance scoped to the session's project (cwd-derived
    // data dir under `/data`), run `new-session` on it directly, then
    // transfer ownership to a [`SessionActor`] spawned on the local set.
    // The guest mints the session id; we register the actor under that id.
    //
    // Outbound `update-session` events emitted *during* `new-session` carry
    // the guest-minted id and route through the shared outbound channel,
    // so they reach the editor even before the registry has the entry.
    let mut agent = factory
        .instantiate_for_project(&req.cwd)
        .await
        .map_err(|e| translate::anyhow_to_acp("new-session: instantiate", e))?;
    let wit_req = translate::new_session_request_schema_to_wit(req);
    let result = agent
        .call_new_session(&wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("new-session", e))?;
    let resp = result.map_err(translate::wit_error_to_acp)?;
    debug!(session = %resp.session_id, "session/new");
    let session_id = resp.session_id.clone();
    let (actor, handle) = SessionActor::new(agent, 8, registry.clone());
    tokio::task::spawn_local(actor.run());
    registry.insert(session_id, handle);
    responder.respond(translate::new_session_response_wit_to_schema(
        resp,
        factory.component_id(),
    )?)
}

async fn handle_load_session(
    factory: &SessionFactory,
    registry: &Arc<SessionRegistry>,
    req: schema::LoadSessionRequest,
    responder: Responder<schema::LoadSessionResponse>,
) -> Result<(), AcpError> {
    let session_key = req.session_id.0.to_string();
    debug!(session = %session_key, "session/load");
    let mut agent = factory
        .instantiate_for_project(&req.cwd)
        .await
        .map_err(|e| translate::anyhow_to_acp("load-session: instantiate", e))?;
    let wit_req = translate::load_session_request_schema_to_wit(req);
    let result = agent
        .call_load_session(&wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("load-session", e))?;
    let resp = result.map_err(translate::wit_error_to_acp)?;
    let (actor, handle) = SessionActor::new(agent, 8, registry.clone());
    tokio::task::spawn_local(actor.run());
    registry.insert(session_key, handle);
    responder.respond(translate::load_session_response_wit_to_schema(
        resp,
        factory.component_id(),
    )?)
}

/// Spawn the wasm round-trip so this handler returns immediately. If we
/// await `handle.set_mode` inline, we block the connection's incoming
/// actor and the editor's replies to outbound `fs/*` requests can't be
/// dequeued — a cross-task deadlock that surfaces as the wasm guest's
/// request timing out even though the editor responded in milliseconds.
fn handle_set_session_mode(
    registry: &SessionRegistry,
    req: schema::SetSessionModeRequest,
    responder: Responder<schema::SetSessionModeResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let session_key = req.session_id.0.to_string();
    debug!(session = %session_key, "session/set_mode");

    let handle = require_session(registry, &session_key)?;
    let wit_req = translate::set_session_mode_request_schema_to_wit(req);

    cx.spawn(async move {
        let outcome = match handle.set_mode(wit_req).await {
            Ok(o) => o,
            Err(_) => {
                let mut e = AcpError::internal_error();
                e.message = format!("session actor for {session_key} is gone");
                return responder.respond_with_error(e);
            }
        };
        match outcome {
            SetModeOutcome::Done => {
                let resp = match translate::empty_set_session_mode_response() {
                    Ok(r) => r,
                    Err(e) => return responder.respond_with_error(e),
                };
                responder.respond(resp)
            }
            SetModeOutcome::Wit(e) => responder.respond_with_error(translate::wit_error_to_acp(e)),
            SetModeOutcome::Trap(e) => {
                responder.respond_with_error(translate::trap_to_acp("set-session-mode", e))
            }
        }
    })?;
    Ok(())
}

/// Spawn: see comment on `handle_set_session_mode`. Prompt is the worst
/// offender — a single turn can drive many `fs/*` round-trips through the
/// editor, all of which need the incoming actor free to dequeue replies.
fn handle_prompt(
    registry: &SessionRegistry,
    req: schema::PromptRequest,
    responder: Responder<schema::PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let session_key = req.session_id.0.to_string();
    debug!(session = %session_key, "session/prompt");

    let handle = require_session(registry, &session_key)?;
    let wit_req = translate::prompt_request_schema_to_wit(req);

    cx.spawn(async move {
        let outcome = match handle.prompt(wit_req).await {
            Ok(o) => o,
            Err(_) => {
                let mut e = AcpError::internal_error();
                e.message = format!("session actor for {session_key} is gone");
                return responder.respond_with_error(e);
            }
        };
        let resp = match outcome {
            PromptOutcome::Done(r) => match translate::prompt_response_wit_to_schema(r) {
                Ok(r) => r,
                Err(e) => return responder.respond_with_error(e),
            },
            PromptOutcome::Cancelled => {
                debug!(session = %session_key, "session/prompt cancelled");
                match translate::synthesised_cancelled_response() {
                    Ok(r) => r,
                    Err(e) => return responder.respond_with_error(e),
                }
            }
            PromptOutcome::Wit(e) => {
                return responder.respond_with_error(translate::wit_error_to_acp(e));
            }
            PromptOutcome::Trap(e) => {
                return responder.respond_with_error(translate::trap_to_acp("prompt", e));
            }
        };
        responder.respond(resp)
    })?;
    Ok(())
}

async fn handle_cancel(
    registry: &SessionRegistry,
    notif: schema::CancelNotification,
) -> Result<(), AcpError> {
    let key = notif.session_id.0.to_string();
    debug!(session = %key, "session/cancel");
    // Signal the in-flight prompt via the actor's out-of-band watch
    // channel. The actor's `tokio::select!` will pick it up and return
    // `Cancelled` for the current turn. We don't attempt to deliver a
    // guest-side `cancel` call here: that's a TODO no-op anyway and would
    // have to queue behind the running prompt.
    if let Some(handle) = registry.get(&key) {
        handle.cancel();
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Outbound drain
// -----------------------------------------------------------------------------

/// Drain wasm-emitted outbound events.
///
/// Notifications go out inline. Outbound requests issue immediately via
/// `cx.send_request` and the `block_task().await` is handed to `cx.spawn`
/// so the drain loop returns to `outbound_rx.recv()` right away.
///
/// Awaiting `block_task` inline would deadlock: it's documented as unsafe
/// to await from anything that's blocking the connection's event loop
/// (handlers, the `connect_with` body). The editor reply lands at the
/// transport actor in milliseconds but the inline await never wakes —
/// only the wasm guest's outer timeout tears the chain down. Running
/// `block_task` inside a `cx.spawn`-ed task is the supported pattern.
async fn run_outbound_drain(
    cx: ConnectionTo<Client>,
    outbound_rx: &mut mpsc::Receiver<OutboundEvent>,
) -> Result<(), AcpError> {
    while let Some(event) = outbound_rx.recv().await {
        match event {
            OutboundEvent::SessionUpdate(notif) => {
                if !forward_session_update(&cx, notif) {
                    break;
                }
            }
            OutboundEvent::ReadTextFile(req, reply) => {
                forward_read_text_file(&cx, req, reply);
            }
            OutboundEvent::WriteTextFile(req, reply) => {
                forward_write_text_file(&cx, req, reply);
            }
        }
    }
    Ok(())
}

/// Forward a `session/update` notification. Returns `false` if the
/// connection has shut down and the drain loop should exit.
fn forward_session_update(cx: &ConnectionTo<Client>, notif: schema::SessionNotification) -> bool {
    if let Err(e) = cx.send_notification(notif) {
        warn!("failed to send session/update: {e:?}");
        return false;
    }
    true
}

fn forward_read_text_file(
    cx: &ConnectionTo<Client>,
    req: schema::ReadTextFileRequest,
    reply: oneshot::Sender<Result<schema::ReadTextFileResponse, AcpError>>,
) {
    let path = req.path.display().to_string();
    let session = req.session_id.0.to_string();
    debug!(session = %session, path = %path, "fs/read_text_file dispatched");
    let pending = cx.send_request(req);
    let _ = cx.spawn(async move {
        let result = pending.block_task().await;
        match &result {
            Ok(_) => debug!(
                session = %session,
                path = %path,
                "fs/read_text_file responded ok"
            ),
            Err(e) => debug!(
                session = %session,
                path = %path,
                error = %e.message,
                "fs/read_text_file responded err"
            ),
        }
        let _ = reply.send(result);
        Ok(())
    });
}

fn forward_write_text_file(
    cx: &ConnectionTo<Client>,
    req: schema::WriteTextFileRequest,
    reply: oneshot::Sender<Result<schema::WriteTextFileResponse, AcpError>>,
) {
    let path = req.path.display().to_string();
    let session = req.session_id.0.to_string();
    debug!(session = %session, path = %path, "fs/write_text_file dispatched");
    let pending = cx.send_request(req);
    let _ = cx.spawn(async move {
        let result = pending.block_task().await;
        match &result {
            Ok(_) => debug!(
                session = %session,
                path = %path,
                "fs/write_text_file responded ok"
            ),
            Err(e) => debug!(
                session = %session,
                path = %path,
                error = %e.message,
                "fs/write_text_file responded err"
            ),
        }
        let _ = reply.send(result);
        Ok(())
    });
}
