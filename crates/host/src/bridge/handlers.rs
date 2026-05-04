//! Per-method handler functions for the ACP bridge.
//!
//! The builder closures in [`super::run`] are thin shims that forward to
//! the named functions here. Stateless calls (`initialize`, `authenticate`)
//! spin up a throwaway wasm instance via [`SessionFactory`]. Session-scoped
//! calls (`set_session_mode`, `prompt`) look up a [`SessionHandle`] in the
//! [`SessionRegistry`] and dispatch to the per-session actor; they `cx.spawn`
//! the wasm round-trip so the handler returns immediately and the
//! connection's incoming actor stays free to dequeue editor replies to
//! outbound `fs/*` requests. Awaiting wasm work inline would deadlock the
//! whole connection.

use std::sync::Arc;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::{ConnectionTo, Error as AcpError, Responder, schema};
use tracing::debug;

use super::require_session;
use crate::translate;
use crate::wasm::{PromptOutcome, SessionActor, SessionFactory, SessionRegistry, SetModeOutcome};

pub(super) async fn handle_initialize(
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

pub(super) async fn handle_authenticate(
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

pub(super) async fn handle_new_session(
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

pub(super) async fn handle_load_session(
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
pub(super) fn handle_set_session_mode(
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
pub(super) fn handle_prompt(
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

pub(super) async fn handle_cancel(
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
