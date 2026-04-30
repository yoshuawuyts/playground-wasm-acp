//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports). Most calls return `method-not-found` for the MVP; only
//! `update-session` is wired through to the host's outbound channel.

use crate::state::HostState;
use crate::translate;
use crate::yoshuawuyts::acp::client;
use crate::yoshuawuyts::acp::types as acp;

impl client::Host for HostState {
    async fn update_session(&mut self, session_id: acp::SessionId, update: acp::SessionUpdate) {
        if let Some(notif) = translate::session_update_wit_to_schema(session_id, update) {
            // Best-effort: if the receiver is gone, the connection has shut
            // down; nothing useful to do here.
            let _ = self.updates.send(notif);
        }
    }

    async fn request_permission(
        &mut self,
        _req: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        Err(translate::method_not_found("request-permission not wired"))
    }

    async fn read_text_file(
        &mut self,
        _req: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(translate::method_not_found("read-text-file not supported"))
    }

    async fn write_text_file(&mut self, _req: acp::WriteTextFileRequest) -> Result<(), acp::Error> {
        Err(translate::method_not_found("write-text-file not supported"))
    }

    async fn create_terminal(
        &mut self,
        _req: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(translate::method_not_found("create-terminal not supported"))
    }

    async fn get_terminal_output(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<acp::TerminalOutput, acp::Error> {
        Err(translate::method_not_found(
            "get-terminal-output not supported",
        ))
    }

    async fn wait_for_terminal_exit(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<acp::TerminalExitStatus, acp::Error> {
        Err(translate::method_not_found(
            "wait-for-terminal-exit not supported",
        ))
    }

    async fn kill_terminal(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<(), acp::Error> {
        Err(translate::method_not_found("kill-terminal not supported"))
    }

    async fn release_terminal(
        &mut self,
        _session_id: acp::SessionId,
        _terminal_id: acp::TerminalId,
    ) -> Result<(), acp::Error> {
        Err(translate::method_not_found(
            "release-terminal not supported",
        ))
    }
}
