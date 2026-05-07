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

use std::sync::atomic::{AtomicBool, Ordering};

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
    AvailableCommand, PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
use acp_wasm_sys::layer::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SetSessionModeRequest,
};
use acp_wasm_sys::layer::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use acp_wasm_sys::layer::yosh::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};
use acp_wasm_sys::layer::yosh::acp::{agent, client};

struct Layer;

/// Whether agent-emitted text should be shouted (uppercased). Toggled
/// in-process via the `/shout` slash command; not persisted across
/// component restarts.
static SHOUT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Push the layer's `available-commands-update` upstream so the editor
/// learns about `/shout`. Sent after each session lifecycle method.
async fn advertise_commands(session_id: &SessionId) {
    let cmds = vec![AvailableCommand {
        name: "shout".to_string(),
        description: "Toggle uppercase rewriting of agent output for this session."
            .to_string(),
        input: None,
    }];
    client::update_session(
        session_id.clone(),
        SessionUpdate::AvailableCommandsUpdate(cmds),
    )
    .await;
}

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

/// Returns true when the prompt's concatenated text content (across
/// any text blocks, ignoring non-text blocks like resource links) is
/// exactly `/shout`.
fn is_shout_command(blocks: &[ContentBlock]) -> bool {
    let mut text = String::new();
    for block in blocks {
        if let ContentBlock::Text(TextContent { text: t }) = block {
            text.push_str(t);
        }
    }
    text.trim() == "/shout"
}

// -----------------------------------------------------------------------------
// agent direction: forward downstream verbatim
// -----------------------------------------------------------------------------

impl AgentGuest for Layer {
    async fn initialize(req: InitializeRequest) -> Result<InitializeResponse, Error> {
        agent::initialize(req).await
    }

    async fn authenticate(req: AuthenticateRequest) -> Result<(), Error> {
        agent::authenticate(req).await
    }

    async fn new_session(req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let resp = agent::new_session(req).await?;
        advertise_commands(&resp.session_id).await;
        Ok(resp)
    }

    async fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let sid = req.session_id.clone();
        let resp = agent::load_session(req).await?;
        advertise_commands(&sid).await;
        Ok(resp)
    }

    async fn list_sessions(req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        agent::list_sessions(req).await
    }

    async fn resume_session(req: ResumeSessionRequest) -> Result<ResumeSessionResponse, Error> {
        let sid = req.session_id.clone();
        let resp = agent::resume_session(req).await?;
        advertise_commands(&sid).await;
        Ok(resp)
    }

    async fn close_session(session_id: SessionId) -> Result<(), Error> {
        agent::close_session(session_id).await
    }

    async fn set_session_mode(req: SetSessionModeRequest) -> Result<(), Error> {
        agent::set_session_mode(req).await
    }

    async fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
        // Intercept `/shout` to toggle uppercase rewriting for the
        // remainder of this session.
        if is_shout_command(&req.prompt) {
            let now_on = !SHOUT_ENABLED.fetch_xor(true, Ordering::Relaxed);
            let msg = if now_on {
                "I AM VERY CALM RIGHT NOW!"
            } else {
                "ok, I've calmed down"
            };
            client::update_session(
                req.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent {
                    text: msg.to_string(),
                })),
            )
            .await;
            return Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            });
        }
        agent::prompt(req).await
    }

    async fn cancel(session_id: SessionId) {
        agent::cancel(session_id).await;
    }
}

// -----------------------------------------------------------------------------
// client direction: rewrite update-session, forward everything else
// -----------------------------------------------------------------------------

impl ClientGuest for Layer {
    async fn update_session(session_id: SessionId, update: SessionUpdate) {
        let rewritten = if SHOUT_ENABLED.load(Ordering::Relaxed) {
            uppercase_update(update)
        } else {
            update
        };
        client::update_session(session_id, rewritten).await;
    }

    async fn request_permission(
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        client::request_permission(req).await
    }

    async fn read_text_file(req: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        client::read_text_file(req).await
    }

    async fn write_text_file(req: WriteTextFileRequest) -> Result<(), Error> {
        client::write_text_file(req).await
    }

    async fn create_terminal(req: CreateTerminalRequest) -> Result<CreateTerminalResponse, Error> {
        client::create_terminal(req).await
    }

    async fn get_terminal_output(
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalOutput, Error> {
        client::get_terminal_output(session_id, terminal_id).await
    }

    async fn wait_for_terminal_exit(
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<TerminalExitStatus, Error> {
        client::wait_for_terminal_exit(session_id, terminal_id).await
    }

    async fn kill_terminal(session_id: SessionId, terminal_id: TerminalId) -> Result<(), Error> {
        client::kill_terminal(session_id, terminal_id).await
    }

    async fn release_terminal(
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> Result<(), Error> {
        client::release_terminal(session_id, terminal_id).await
    }
}

acp_wasm_sys::layer::export!(Layer with_types_in acp_wasm_sys::layer);
