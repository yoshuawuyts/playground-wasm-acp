//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports). For methods that need to talk to the editor, we package the
//! request onto an `OutboundEvent` channel that the bridge task drains.

use agent_client_protocol::Error as AcpError;
use tokio::sync::{mpsc, oneshot};

use crate::state::{HostState, OutboundEvent};
use crate::translate;
use crate::yoshuawuyts::acp::client;
use crate::yoshuawuyts::acp::errors::Error;
use crate::yoshuawuyts::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use crate::yoshuawuyts::acp::prompts::SessionUpdate;
use crate::yoshuawuyts::acp::sessions::SessionId;
use crate::yoshuawuyts::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use crate::yoshuawuyts::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};

/// Send an outbound event and await the bridge task's reply, translating
/// any transport-level failure (channel closed, no response) into an ACP
/// error suitable for returning to the wasm guest.
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
    match rx.await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(acp_err)) => Err(translate::acp_error_to_wit(acp_err)),
        Err(_) => Err(translate::internal_error(&format!(
            "{context}: bridge dropped reply"
        ))),
    }
}

impl client::Host for HostState {
    async fn update_session(&mut self, session_id: SessionId, update: SessionUpdate) {
        if let Some(notif) = translate::session_update_wit_to_schema(session_id, update) {
            // Best-effort: if the receiver is gone, the connection has shut
            // down; nothing useful to do here. Use bounded `send` so we
            // backpressure the wasm guest if the editor is slow.
            let _ = self
                .outbound
                .send(OutboundEvent::SessionUpdate(notif))
                .await;
        }
    }

    async fn request_permission(
        &mut self,
        _req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        Err(translate::method_not_found("request-permission not wired"))
    }

    async fn read_text_file(
        &mut self,
        req: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, Error> {
        let schema_req = translate::read_text_file_request_wit_to_schema(req);
        let outbound = self.outbound.clone();
        let resp = send_and_await(
            &outbound,
            |tx| OutboundEvent::ReadTextFile(schema_req, tx),
            "fs/read",
        )
        .await?;
        Ok(translate::read_text_file_response_schema_to_wit(resp))
    }

    async fn write_text_file(&mut self, req: WriteTextFileRequest) -> Result<(), Error> {
        let schema_req = translate::write_text_file_request_wit_to_schema(req);
        let outbound = self.outbound.clone();
        send_and_await(
            &outbound,
            |tx| OutboundEvent::WriteTextFile(schema_req, tx),
            "fs/write",
        )
        .await?;
        Ok(())
    }

    async fn create_terminal(
        &mut self,
        _req: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, Error> {
        Err(translate::method_not_found("create-terminal not supported"))
    }

    async fn get_terminal_output(
        &mut self,
        _session_id: SessionId,
        _terminal_id: TerminalId,
    ) -> Result<TerminalOutput, Error> {
        Err(translate::method_not_found(
            "get-terminal-output not supported",
        ))
    }

    async fn wait_for_terminal_exit(
        &mut self,
        _session_id: SessionId,
        _terminal_id: TerminalId,
    ) -> Result<TerminalExitStatus, Error> {
        Err(translate::method_not_found(
            "wait-for-terminal-exit not supported",
        ))
    }

    async fn kill_terminal(
        &mut self,
        _session_id: SessionId,
        _terminal_id: TerminalId,
    ) -> Result<(), Error> {
        Err(translate::method_not_found("kill-terminal not supported"))
    }

    async fn release_terminal(
        &mut self,
        _session_id: SessionId,
        _terminal_id: TerminalId,
    ) -> Result<(), Error> {
        Err(translate::method_not_found(
            "release-terminal not supported",
        ))
    }
}
