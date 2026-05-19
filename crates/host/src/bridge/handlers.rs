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

use super::gate::NotificationGate;
use super::require_session;
use crate::translate;
use crate::wasm::{PromptOutcome, SessionActor, SessionFactory, SessionRegistry, SetModeOutcome};

pub(super) async fn handle_initialize(
    factory: &SessionFactory,
    req: schema::InitializeRequest,
    responder: Responder<schema::InitializeResponse>,
) -> Result<(), AcpError> {
    // Throwaway instance: `initialize` carries no session state.
    let chain = factory
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
    let result = chain
        .head
        .call_initialize(wit_req)
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
    let chain = factory
        .instantiate()
        .await
        .map_err(|e| translate::anyhow_to_acp("authenticate: instantiate", e))?;
    let wit_req = translate::authenticate_request_schema_to_wit(req);
    let result = chain
        .head
        .call_authenticate(wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("authenticate", e))?;
    result.map_err(translate::wit_error_to_acp)?;
    responder.respond(translate::empty_authenticate_response()?)
}

pub(super) async fn handle_new_session(
    factory: &SessionFactory,
    registry: &Arc<SessionRegistry>,
    gate: &Arc<NotificationGate>,
    mut req: schema::NewSessionRequest,
    responder: Responder<schema::NewSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), AcpError> {
    // Spin up a fresh instance scoped to the session's project (cwd-derived
    // data dir under `/data`), run `new-session` on it directly, then
    // transfer ownership to a [`SessionActor`] spawned on the local set.
    // The guest mints the session id; we register the actor under that id.
    //
    // Outbound `update-session` events emitted *during* `new-session` carry
    // the guest-minted id and route through the shared outbound channel,
    // so they reach the editor even before the registry has the entry.
    if let Ok(payload) = serde_json::to_string(&req) {
        tracing::info!(payload = %payload, "← wire: session/new");
    }
    resolve_workspace_cwd(&mut req.cwd);
    warn_if_unlikely_workspace(&req.cwd);
    let chain = factory
        .instantiate_for_project(&req.cwd)
        .await
        .map_err(|e| translate::anyhow_to_acp("new-session: instantiate", e))?;
    let wit_req = translate::new_session_request_schema_to_wit(req);
    let result = chain
        .head
        .call_new_session(wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("new-session", e))?;
    let resp = result.map_err(translate::wit_error_to_acp)?;
    debug!(session = %resp.session_id, "session/new");
    let session_id = resp.session_id.clone();
    let (actor, handle) = SessionActor::new(chain, 8, registry.clone());
    tokio::task::spawn_local(actor.run());
    registry.insert(session_id.clone(), handle);
    let schema_resp = translate::new_session_response_wit_to_schema(resp, factory.component_id())?;
    if let Ok(payload) = serde_json::to_string(&schema_resp) {
        tracing::info!(payload = %payload, "→ wire: session/new response");
    }
    responder.respond(schema_resp)?;
    // Now that the session/new response has been sent, release any
    // notifications the chain emitted *during* the call (e.g. a layer
    // advertising slash commands). Sending them earlier would race the
    // response and the editor would drop them as referring to an
    // unknown session id.
    flush_held_notifications(gate, &session_id, &cx);
    Ok(())
}

pub(super) async fn handle_load_session(
    factory: &SessionFactory,
    registry: &Arc<SessionRegistry>,
    gate: &Arc<NotificationGate>,
    req: schema::LoadSessionRequest,
    responder: Responder<schema::LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), AcpError> {
    let session_key = req.session_id.0.to_string();
    debug!(session = %session_key, "session/load");
    warn_if_unlikely_workspace(&req.cwd);
    let chain = factory
        .instantiate_for_project(&req.cwd)
        .await
        .map_err(|e| translate::anyhow_to_acp("load-session: instantiate", e))?;
    let wit_req = translate::load_session_request_schema_to_wit(req);
    let result = chain
        .head
        .call_load_session(wit_req)
        .await
        .map_err(|e| translate::trap_to_acp("load-session", e))?;
    let resp = result.map_err(translate::wit_error_to_acp)?;
    let (actor, handle) = SessionActor::new(chain, 8, registry.clone());
    tokio::task::spawn_local(actor.run());
    registry.insert(session_key.clone(), handle);
    responder.respond(translate::load_session_response_wit_to_schema(
        resp,
        factory.component_id(),
    )?)?;
    flush_held_notifications(gate, &session_key, &cx);
    Ok(())
}

/// After responding to `session/new` or `session/load`, mark the
/// session as opened in the gate and forward any notifications that
/// were buffered while the wasm chain processed the call.
///
/// We deliberately delay the flush by a few hundred milliseconds. The
/// editor reads our `session/new` response and any `session/update`
/// notification from the same stdio stream into separate async tasks;
/// if the notification task is polled before the editor's response
/// handler finishes registering its session-side thread, the update is
/// looked up against an empty session map and silently dropped. The
/// concrete user-visible symptom in Zed is "Available commands: none"
/// even though the layer advertised commands at session start. A small
/// delay reliably gives the editor's response handler time to wire up
/// the session before our held notifications arrive.
fn flush_held_notifications(
    gate: &Arc<NotificationGate>,
    session_id: &str,
    cx: &ConnectionTo<Client>,
) {
    let gate = gate.clone();
    let session_id = session_id.to_string();
    let cx_inner = cx.clone();
    let _ = cx.spawn(async move {
        // 200ms is comfortably above the inter-task scheduling latency
        // we've observed in Zed and small enough to feel instantaneous.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        for notif in gate.open_session(&session_id) {
            if let Ok(json) = serde_json::to_string(&notif) {
                tracing::info!(payload = %json, "→ wire: flushed session/update");
            }
            if let Err(e) = cx_inner.send_notification(notif) {
                tracing::warn!(error = ?e, "failed to flush held session/update");
                break;
            }
        }
        Ok(())
    });
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
    if let Ok(payload) = serde_json::to_string(&req) {
        tracing::info!(session = %session_key, payload = %payload, "← wire: session/prompt");
    }

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

/// Normalize a session `cwd` provided by the editor. Today this only
/// canonicalizes relative paths against the host process's working
/// directory — absolute paths are left alone. Editors are supposed to
/// send an absolute path, but some don't; making it absolute up front
/// keeps every downstream consumer (data dir derivation, wasm preopens,
/// tool-call path resolution) on the same footing.
fn resolve_workspace_cwd(cwd: &mut std::path::PathBuf) {
    if cwd.is_absolute() {
        return;
    }
    if let Ok(here) = std::env::current_dir() {
        *cwd = here.join(&*cwd);
    }
}

/// Emit a one-time `tracing::warn` if the editor's session `cwd` doesn't
/// look like a project root (no common project markers found, or the
/// path is the user's `$HOME`). This is the most frequent cause of
/// "tools don't work" demo failures: the editor was launched outside a
/// project, every relative `read_file` resolves under `$HOME`, and
/// nothing is found.
fn warn_if_unlikely_workspace(cwd: &std::path::Path) {
    if !cwd.is_absolute() {
        tracing::warn!(cwd = %cwd.display(), "session cwd is not absolute; tool calls with relative paths will likely fail");
        return;
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if home.as_deref() == Some(cwd) {
        tracing::warn!(
            cwd = %cwd.display(),
            "session cwd is $HOME; the editor was likely launched outside a project. \
             Relative paths from the model (e.g. `README.md`) will resolve under $HOME \
             and almost certainly miss. Open the editor inside a project directory."
        );
        return;
    }
    const MARKERS: &[&str] = &[
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "deno.json",
        "tsconfig.json",
    ];
    let has_marker = MARKERS.iter().any(|m| cwd.join(m).exists());
    if !has_marker {
        tracing::warn!(
            cwd = %cwd.display(),
            "session cwd has no obvious project markers ({}); model tool calls with \
             relative paths may not resolve to anything useful",
            MARKERS.join(", ")
        );
    }
}
