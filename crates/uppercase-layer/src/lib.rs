//! ACP layer that uppercases all agent-emitted text flowing toward the
//! editor. Every other call is forwarded verbatim to the next stage.
//!
//! Agent direction (downstream): forward as-is via the imported `agent`
//! interface.
//!
//! Client direction (upstream): forward via the imported `client`
//! interface, but rewrite `update-session` payloads so any text inside
//! agent message / thought chunks is uppercased before it bubbles up to
//! the host.

#![allow(clippy::too_many_arguments)]

use acp_wasm_sys::layer::exports::yosh::acp::agent::Guest as AgentGuest;
use acp_wasm_sys::layer::exports::yosh::acp::client::Guest as ClientGuest;
use acp_wasm_sys::layer::yosh::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::layer::yosh::acp::errors::Error;
use acp_wasm_sys::layer::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use acp_wasm_sys::layer::yosh::acp::init::{
    AuthenticateRequest, InitializeRequest, InitializeResponse,
};
use acp_wasm_sys::layer::yosh::acp::prompts::{
    PromptRequest, PromptResponse, SessionUpdate,
};
use acp_wasm_sys::layer::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SetSessionModeRequest,
};
use acp_wasm_sys::layer::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use acp_wasm_sys::layer::yosh::acp::tools::{
    RequestPermissionRequest, RequestPermissionResponse,
};
use acp_wasm_sys::layer::yosh::acp::{agent, client};

struct Layer;

/// Uppercase the `text` field of any `ContentBlock::Text`. Other content
/// variants (image, audio, resource) pass through unchanged — they don't
/// carry user-visible prose to transform.
fn uppercase_block(block: ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Text(TextContent { text }) => ContentBlock::Text(TextContent {
            text: text.to_uppercase(),
        }),
        other => other,
    }
}

/// Rewrite an outbound `session/update` so any agent-authored text is
/// uppercased. User-message replays and tool-call payloads are left
/// alone: the layer's purpose is to mangle what the *agent* says, not
/// what the user wrote or what tools report.
fn uppercase_update(update: SessionUpdate) -> SessionUpdate {
    match update {
        SessionUpdate::AgentMessageChunk(b) => SessionUpdate::AgentMessageChunk(uppercase_block(b)),
        SessionUpdate::AgentThoughtChunk(b) => SessionUpdate::AgentThoughtChunk(uppercase_block(b)),
        other => other,
    }
}

// -----------------------------------------------------------------------------
// agent direction: forward downstream verbatim
// -----------------------------------------------------------------------------

impl AgentGuest for Layer {
    fn initialize(req: InitializeRequest) -> Result<InitializeResponse, Error> {
        agent::initialize(&req)
    }

    fn authenticate(req: AuthenticateRequest) -> Result<(), Error> {
        agent::authenticate(&req)
    }

    fn new_session(req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        agent::new_session(&req)
    }

    fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        agent::load_session(&req)
    }

    fn list_sessions(req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        agent::list_sessions(&req)
    }

    fn resume_session(req: ResumeSessionRequest) -> Result<ResumeSessionResponse, Error> {
        agent::resume_session(&req)
    }

    fn close_session(session_id: SessionId) -> Result<(), Error> {
        agent::close_session(&session_id)
    }

    fn set_session_mode(req: SetSessionModeRequest) -> Result<(), Error> {
        agent::set_session_mode(&req)
    }

    fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
        agent::prompt(&req)
    }

    fn cancel(session_id: SessionId) {
        agent::cancel(&session_id);
    }
}

// -----------------------------------------------------------------------------
// client direction: rewrite update-session, forward everything else
// -----------------------------------------------------------------------------

impl ClientGuest for Layer {
    fn update_session(session_id: SessionId, update: SessionUpdate) {
        let rewritten = uppercase_update(update);
        client::update_session(&session_id, &rewritten);
    }

    fn request_permission(
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        client::request_permission(&req)
    }

    fn read_text_file(req: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        client::read_text_file(&req)
    }

    fn write_text_file(req: WriteTextFileRequest) -> Result<(), Error> {
        client::write_text_file(&req)
    }

    fn create_terminal(req: CreateTerminalRequest) -> Result<CreateTerminalResponse, Error> {
        client::create_terminal(&req)
    }

    fn get_terminal_output(
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalOutput, Error> {
        client::get_terminal_output(&session_id, &terminal_id)
    }

    fn wait_for_terminal_exit(
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalExitStatus, Error> {
        client::wait_for_terminal_exit(&session_id, &terminal_id)
    }

    fn kill_terminal(session_id: SessionId, terminal_id: TerminalId) -> Result<(), Error> {
        client::kill_terminal(&session_id, &terminal_id)
    }

    fn release_terminal(session_id: SessionId, terminal_id: TerminalId) -> Result<(), Error> {
        client::release_terminal(&session_id, &terminal_id)
    }
}

acp_wasm_sys::layer::export!(Layer with_types_in acp_wasm_sys::layer);
