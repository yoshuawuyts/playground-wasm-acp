//! Wires the ACP `agent_client_protocol::Builder` over stdio and dispatches
//! incoming JSON-RPC messages into the wasm component.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::role::acp::Agent as AgentRole;
use agent_client_protocol::{ByteStreams, schema};
use anyhow::Result;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, warn};

use crate::state::OutboundEvent;
use crate::translate;
use crate::wasm::WasmAgent;

/// Per-session cancellation flag. The cancel handler signals; the prompt
/// handler watches.
type CancelMap = Arc<std::sync::Mutex<HashMap<String, watch::Sender<bool>>>>;

/// Run the ACP bridge to completion. Returns when the editor disconnects
/// (stdin EOF) or an error occurs.
pub async fn run(
    agent: Arc<Mutex<WasmAgent>>,
    mut outbound_rx: mpsc::Receiver<OutboundEvent>,
) -> Result<()> {
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );

    let cancels: CancelMap = Arc::new(std::sync::Mutex::new(HashMap::new()));

    let agent_init = agent.clone();
    let agent_auth = agent.clone();
    let agent_new = agent.clone();
    let agent_load = agent.clone();
    let agent_prompt = agent.clone();
    let agent_cancel = agent.clone();
    let cancels_prompt = cancels.clone();
    let cancels_cancel = cancels.clone();

    AgentRole
        .builder()
        .name("ollama-wasm-host")
        .on_receive_request(
            async move |req: schema::InitializeRequest, responder, _cx| {
                let mut a = agent_init.lock().await;
                let wit_req = translate::init_request_schema_to_wit(req);
                let result = a
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
                let mut a = agent_auth.lock().await;
                let wit_req = translate::authenticate_request_schema_to_wit(req);
                let result = a
                    .call_authenticate(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("authenticate", e))?;
                result.map_err(translate::wit_error_to_acp)?;
                let empty: schema::AuthenticateResponse =
                    serde_json::from_value(serde_json::json!({})).expect("empty auth response");
                responder.respond(empty)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::NewSessionRequest, responder, _cx| {
                let mut a = agent_new.lock().await;
                let wit_req = translate::new_session_request_schema_to_wit(req);
                let result = a
                    .call_new_session(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("new-session", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::new_session_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::LoadSessionRequest, responder, _cx| {
                let mut a = agent_load.lock().await;
                let wit_req = translate::load_session_request_schema_to_wit(req);
                let result = a
                    .call_load_session(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("load-session", e))?;
                result.map_err(translate::wit_error_to_acp)?;
                let empty: schema::LoadSessionResponse =
                    serde_json::from_value(serde_json::json!({})).expect("empty load response");
                responder.respond(empty)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: schema::PromptRequest, responder, _cx| {
                let session_key = req.session_id.0.to_string();
                debug!(session = %session_key, "session/prompt");

                // Register a cancel watch for this turn so a concurrent
                // session/cancel can interrupt our wasm-side `await`s.
                let (cancel_tx, mut cancel_rx) = watch::channel(false);
                cancels_prompt
                    .lock()
                    .unwrap()
                    .insert(session_key.clone(), cancel_tx);

                let result = {
                    let mut a = agent_prompt.lock().await;
                    let wit_req = translate::prompt_request_schema_to_wit(req);
                    tokio::select! {
                        biased;
                        // If cancel arrives while we're awaiting the wasm
                        // call, drop the future and synthesise the spec-
                        // mandated `cancelled` stop reason.
                        _ = cancel_rx.changed() => {
                            Ok(translate::synthesised_cancelled_response())
                        }
                        r = a.call_prompt(&wit_req) => match r {
                            Err(e) => Err(translate::trap_to_acp("prompt", e)),
                            Ok(Err(e)) => Err(translate::wit_error_to_acp(e)),
                            Ok(Ok(resp)) => Ok(translate::prompt_response_wit_to_schema(resp)),
                        }
                    }
                };

                cancels_prompt.lock().unwrap().remove(&session_key);

                responder.respond(result?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::CancelNotification, _cx| {
                let key = notif.session_id.0.to_string();
                // Signal an in-flight prompt for this session, if any. We
                // also forward to the wasm guest in case it has any
                // cooperative cancellation logic of its own (currently a
                // no-op there).
                if let Some(tx) = cancels_cancel.lock().unwrap().get(&key) {
                    let _ = tx.send(true);
                }
                let mut a = agent_cancel.lock().await;
                let sid = translate::cancel_session_id_schema_to_wit(&notif);
                a.call_cancel(&sid).await.ok();
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
