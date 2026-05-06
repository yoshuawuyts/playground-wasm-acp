//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports). Each call is dispatched on [`HostState::client_sink`]:
//!
//! * [`ClientSink::Outbound`] — the calling stage is the topmost in the
//!   chain. The call is packaged onto an [`OutboundEvent`] channel that
//!   the bridge task drains and forwards to the editor.
//! * [`ClientSink::Upstream`] — the calling stage has a layer above it.
//!   The call is forwarded into the upstream layer's `client_inst` (a
//!   *separate* wasm store from whichever one the host trait is currently
//!   running on, so reentrancy isn't an issue). The layer's wasm code can
//!   transform the call, then re-emits it via its own `client` import,
//!   bubbling up the chain until it reaches the topmost
//!   `ClientSink::Outbound`.

use agent_client_protocol::Error as AcpError;
use tokio::sync::{mpsc, oneshot};

use crate::state::{ClientSink, HostState, OutboundEvent};
use crate::translate;
use crate::yosh::acp::client;
use crate::yosh::acp::errors::Error;
use crate::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use crate::yosh::acp::prompts::SessionUpdate;
use crate::yosh::acp::sessions::SessionId;
use crate::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use crate::yosh::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};

/// Hard ceiling on how long we wait for the editor to reply to an outbound
/// request. The ACP protocol has no built-in timeout; without this, a buggy
/// or slow editor can wedge a wasm session forever (e.g. a `read_text_file`
/// on a path the editor doesn't recognise but also doesn't error on).
///
/// 10s is a guess; tune later. If a tool legitimately needs longer, we
/// should add a per-call override rather than raise this globally.
const OUTBOUND_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Send an outbound event and await the bridge task's reply, translating
/// any transport-level failure (channel closed, no response, timeout)
/// into an ACP error suitable for returning to the wasm guest.
async fn send_and_await<T>(
    outbound: &mpsc::Sender<OutboundEvent>,
    make_event: impl FnOnce(oneshot::Sender<Result<T, AcpError>>) -> OutboundEvent,
    context: &'static str,
) -> Result<T, Error> {
    let (tx, rx) = oneshot::channel();
    // Bounded send: if the bridge task is backed up, this awaits — natural
    // backpressure into the wasm guest.
    outbound
        .send(make_event(tx))
        .await
        .map_err(|_| translate::internal_error(&format!("{context}: bridge task gone")))?;
    match tokio::time::timeout(OUTBOUND_REQUEST_TIMEOUT, rx).await {
        Ok(Ok(Ok(resp))) => Ok(resp),
        Ok(Ok(Err(acp_err))) => Err(translate::acp_error_to_wit(acp_err)),
        Ok(Err(_)) => Err(translate::internal_error(&format!(
            "{context}: bridge dropped reply"
        ))),
        Err(_) => Err(translate::internal_error(&format!(
            "{context}: editor did not respond within {}s",
            OUTBOUND_REQUEST_TIMEOUT.as_secs()
        ))),
    }
}

/// Collapse `wasmtime::Result<Result<T, Error>>` into `Result<T, Error>`
/// for the host trait return types. A trap in the upstream layer becomes
/// a WIT `internal-error` so the calling stage sees a recoverable error
/// rather than tearing down the chain.
fn flatten_upstream<T>(
    method: &'static str,
    res: wasmtime::Result<Result<T, Error>>,
) -> Result<T, Error> {
    match res {
        Ok(inner) => inner,
        Err(trap) => Err(translate::internal_error(&format!(
            "upstream `{method}` trapped: {trap:#}"
        ))),
    }
}

impl client::Host for HostState {
    async fn update_session(&mut self, session_id: SessionId, update: SessionUpdate) {
        match self.client_sink.clone() {
            ClientSink::Outbound(outbound) => {
                let Some(notif) = translate::session_update_wit_to_schema(session_id, update)
                else {
                    return;
                };
                // Best-effort: if the receiver is gone, the connection has shut
                // down; nothing useful to do here. Use bounded `send` so we
                // backpressure the wasm guest if the editor is slow.
                let _ = outbound.send(OutboundEvent::SessionUpdate(notif)).await;
            }
            ClientSink::Upstream(upstream) => {
                // `update-session` is a one-way notification. If the
                // upstream layer traps we just log: there's no error
                // channel to surface it on.
                if let Err(trap) = upstream
                    .lock()
                    .await
                    .call_client_update_session(&session_id, &update)
                    .await
                {
                    tracing::warn!(error = %trap, "upstream `client.update-session` trapped");
                }
            }
        }
    }

    async fn request_permission(
        &mut self,
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => {
                Err(translate::method_not_found("request-permission not wired"))
            }
            ClientSink::Upstream(upstream) => flatten_upstream(
                "request-permission",
                upstream
                    .lock()
                    .await
                    .call_client_request_permission(&req)
                    .await,
            ),
        }
    }

    async fn read_text_file(
        &mut self,
        req: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(outbound) => {
                let schema_req = translate::read_text_file_request_wit_to_schema(req);
                let resp = send_and_await(
                    &outbound,
                    |tx| OutboundEvent::ReadTextFile(schema_req, tx),
                    "fs/read",
                )
                .await?;
                Ok(translate::read_text_file_response_schema_to_wit(resp))
            }
            ClientSink::Upstream(upstream) => flatten_upstream(
                "read-text-file",
                upstream.lock().await.call_client_read_text_file(&req).await,
            ),
        }
    }

    async fn write_text_file(&mut self, req: WriteTextFileRequest) -> Result<(), Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(outbound) => {
                let schema_req = translate::write_text_file_request_wit_to_schema(req);
                send_and_await(
                    &outbound,
                    |tx| OutboundEvent::WriteTextFile(schema_req, tx),
                    "fs/write",
                )
                .await?;
                Ok(())
            }
            ClientSink::Upstream(upstream) => flatten_upstream(
                "write-text-file",
                upstream
                    .lock()
                    .await
                    .call_client_write_text_file(&req)
                    .await,
            ),
        }
    }

    async fn create_terminal(
        &mut self,
        req: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => {
                Err(translate::method_not_found("create-terminal not supported"))
            }
            ClientSink::Upstream(upstream) => flatten_upstream(
                "create-terminal",
                upstream
                    .lock()
                    .await
                    .call_client_create_terminal(&req)
                    .await,
            ),
        }
    }

    async fn get_terminal_output(
        &mut self,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalOutput, Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => Err(translate::method_not_found(
                "get-terminal-output not supported",
            )),
            ClientSink::Upstream(upstream) => flatten_upstream(
                "get-terminal-output",
                upstream
                    .lock()
                    .await
                    .call_client_get_terminal_output(&session_id, &terminal_id)
                    .await,
            ),
        }
    }

    async fn wait_for_terminal_exit(
        &mut self,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalExitStatus, Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => Err(translate::method_not_found(
                "wait-for-terminal-exit not supported",
            )),
            ClientSink::Upstream(upstream) => flatten_upstream(
                "wait-for-terminal-exit",
                upstream
                    .lock()
                    .await
                    .call_client_wait_for_terminal_exit(&session_id, &terminal_id)
                    .await,
            ),
        }
    }

    async fn kill_terminal(
        &mut self,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<(), Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => {
                Err(translate::method_not_found("kill-terminal not supported"))
            }
            ClientSink::Upstream(upstream) => flatten_upstream(
                "kill-terminal",
                upstream
                    .lock()
                    .await
                    .call_client_kill_terminal(&session_id, &terminal_id)
                    .await,
            ),
        }
    }

    async fn release_terminal(
        &mut self,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<(), Error> {
        match self.client_sink.clone() {
            ClientSink::Outbound(_) => Err(translate::method_not_found(
                "release-terminal not supported",
            )),
            ClientSink::Upstream(upstream) => flatten_upstream(
                "release-terminal",
                upstream
                    .lock()
                    .await
                    .call_client_release_terminal(&session_id, &terminal_id)
                    .await,
            ),
        }
    }
}
