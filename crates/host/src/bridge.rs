//! Wires the ACP `agent_client_protocol::Builder` over stdio and dispatches
//! incoming JSON-RPC messages into the wasm component.

use std::sync::Arc;

use agent_client_protocol::role::acp::Agent as AgentRole;
use agent_client_protocol::{ByteStreams, schema};
use anyhow::Result;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, warn};

use crate::translate;
use crate::wasm::WasmAgent;

/// Run the ACP bridge to completion. Returns when the editor disconnects
/// (stdin EOF) or an error occurs.
pub async fn run(
    agent: Arc<Mutex<WasmAgent>>,
    mut updates_rx: mpsc::UnboundedReceiver<schema::SessionNotification>,
) -> Result<()> {
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );

    let agent_init = agent.clone();
    let agent_auth = agent.clone();
    let agent_new = agent.clone();
    let agent_load = agent.clone();
    let agent_prompt = agent.clone();
    let agent_cancel = agent.clone();

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
                debug!(session = %req.session_id.0, "session/prompt");
                let mut a = agent_prompt.lock().await;
                let wit_req = translate::prompt_request_schema_to_wit(req);
                let result = a
                    .call_prompt(&wit_req)
                    .await
                    .map_err(|e| translate::trap_to_acp("prompt", e))?;
                let resp = result.map_err(translate::wit_error_to_acp)?;
                responder.respond(translate::prompt_response_wit_to_schema(resp))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::CancelNotification, _cx| {
                let mut a = agent_cancel.lock().await;
                let sid = translate::cancel_session_id_schema_to_wit(&notif);
                a.call_cancel(&sid).await.ok();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            // Drain wasm-emitted session updates and forward as JSON-RPC
            // notifications to the client (editor) until the channel closes.
            while let Some(notif) = updates_rx.recv().await {
                if let Err(e) = cx.send_notification(notif) {
                    warn!("failed to send session/update: {e:?}");
                    break;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("acp connection error: {e:?}"))?;

    Ok(())
}
