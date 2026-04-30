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

use agent_client_protocol::role::acp::Agent as AgentRole;
use agent_client_protocol::{ByteStreams, Error as AcpError, schema};
use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, warn};

use crate::state::OutboundEvent;
use crate::translate;
use crate::wasm::{PromptOutcome, SessionActor, SessionFactory, SessionHandle, SessionRegistry};

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
    // the closures `'static`.
    let factory_init = factory.clone();
    let factory_auth = factory.clone();
    let factory_new = factory.clone();
    let factory_load = factory.clone();
    let registry_new = registry.clone();
    let registry_load = registry.clone();
    let registry_prompt = registry.clone();
    let registry_cancel = registry.clone();

    AgentRole
        .builder()
        .name("ollama-wasm-host")
        .on_receive_request(
            async move |req: schema::InitializeRequest, responder, _cx| {
                // Throwaway instance: `initialize` carries no session state.
                let mut agent = factory_init
                    .instantiate()
                    .await
                    .map_err(|e| translate::anyhow_to_acp("initialize: instantiate", e))?;
                let wit_req = translate::init_request_schema_to_wit(req);
                let result = agent
                    .call_initialize(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("initialize", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::init_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::AuthenticateRequest, responder, _cx| {
                // Throwaway instance: `authenticate` is stateless; the host
                // doesn't carry credentials between calls.
                let mut agent = factory_auth
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
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::NewSessionRequest, responder, _cx| {
                // Spin up a fresh instance scoped to the session's project
                // (cwd-derived data dir under `/data`), run `new-session`
                // on it directly, then transfer ownership to a
                // [`SessionActor`] spawned on the local set. The guest
                // mints the session id; we register the actor under that
                // id.
                //
                // Outbound `update-session` events emitted *during*
                // `new-session` carry the guest-minted id and route
                // through the shared outbound channel, so they reach the
                // editor even before the registry has the entry.
                let mut agent = factory_new
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
                let (actor, handle) = SessionActor::new(agent, 8, registry_new.clone());
                tokio::task::spawn_local(actor.run());
                registry_new.insert(session_id, handle);
                responder.respond(translate::new_session_response_wit_to_schema(resp)?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::LoadSessionRequest, responder, _cx| {
                let session_key = req.session_id.0.to_string();
                debug!(session = %session_key, "session/load");
                let mut agent = factory_load
                    .instantiate_for_project(&req.cwd)
                    .await
                    .map_err(|e| translate::anyhow_to_acp("load-session: instantiate", e))?;
                let wit_req = translate::load_session_request_schema_to_wit(req);
                let result = agent
                    .call_load_session(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("load-session", e))?;
                // We discard `LoadSessionResponse.modes` for now; mode
                // plumbing through the WIT translation is Phase 2.
                let _ = result.map_err(translate::wit_error_to_acp)?;
                let (actor, handle) = SessionActor::new(agent, 8, registry_load.clone());
                tokio::task::spawn_local(actor.run());
                registry_load.insert(session_key, handle);
                responder.respond(translate::empty_load_session_response()?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::PromptRequest, responder, _cx| {
                let session_key = req.session_id.0.to_string();
                debug!(session = %session_key, "session/prompt");

                let handle = require_session(&registry_prompt, &session_key)?;
                let wit_req = translate::prompt_request_schema_to_wit(req);

                let outcome = handle.prompt(wit_req).await.map_err(|_| {
                    let mut e = AcpError::internal_error();
                    e.message = format!("session actor for {session_key} is gone");
                    e
                })?;

                let resp = match outcome {
                    PromptOutcome::Done(r) => translate::prompt_response_wit_to_schema(r)?,
                    PromptOutcome::Cancelled => {
                        debug!(session = %session_key, "session/prompt cancelled");
                        translate::synthesised_cancelled_response()?
                    }
                    PromptOutcome::Wit(e) => return Err(translate::wit_error_to_acp(e)),
                    PromptOutcome::Trap(e) => return Err(translate::trap_to_acp("prompt", e)),
                };

                responder.respond(resp)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::CancelNotification, _cx| {
                let key = notif.session_id.0.to_string();
                debug!(session = %key, "session/cancel");
                // Signal the in-flight prompt via the actor's out-of-band
                // watch channel. The actor's `tokio::select!` will pick it
                // up and return `Cancelled` for the current turn. We don't
                // attempt to deliver a guest-side `cancel` call here:
                // that's a TODO no-op anyway and would have to queue
                // behind the running prompt.
                if let Some(handle) = registry_cancel.get(&key) {
                    handle.cancel();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            // Drain wasm-emitted outbound events. Notifications are sent
            // directly; fs requests are spawned so the dispatch loop can
            // serve their responses concurrently with our drain loop.
            while let Some(event) = outbound_rx.recv().await {
                match event {
                    OutboundEvent::SessionUpdate(notif) => {
                        if let Err(e) = cx.send_notification(notif) {
                            warn!("failed to send session/update: {e:?}");
                            break;
                        }
                    }
                    OutboundEvent::ReadTextFile(req, reply) => {
                        let sent = cx.send_request(req);
                        if let Err(e) = cx.spawn(async move {
                            let result = sent.block_task().await;
                            let _ = reply.send(result);
                            Ok(())
                        }) {
                            warn!("failed to spawn fs/read responder: {e:?}");
                        }
                    }
                    OutboundEvent::WriteTextFile(req, reply) => {
                        let sent = cx.send_request(req);
                        if let Err(e) = cx.spawn(async move {
                            let result = sent.block_task().await;
                            let _ = reply.send(result);
                            Ok(())
                        }) {
                            warn!("failed to spawn fs/write responder: {e:?}");
                        }
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("acp connection error: {e:?}"))?;

    Ok(())
}
