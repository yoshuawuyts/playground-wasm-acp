//! Drains wasm-emitted [`OutboundEvent`]s and forwards them to the editor
//! over the ACP connection.
//!
//! Notifications go out inline. Outbound requests issue immediately via
//! `cx.send_request` and the `block_task().await` is handed to `cx.spawn`
//! so the drain loop returns to `outbound_rx.recv()` right away.
//!
//! Awaiting `block_task` inline would deadlock: it's documented as unsafe
//! to await from anything that's blocking the connection's event loop
//! (handlers, the `connect_with` body). The editor reply lands at the
//! transport actor in milliseconds but the inline await never wakes —
//! only the wasm guest's outer timeout tears the chain down. Running
//! `block_task` inside a `cx.spawn`-ed task is the supported pattern.

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::{ConnectionTo, Error as AcpError, schema};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::state::OutboundEvent;

pub(super) async fn run_outbound_drain(
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
