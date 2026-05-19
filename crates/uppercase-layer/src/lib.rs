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

use acp_wasm_sys::layer::exports::yosh::acp::agent::{
    Guest as AgentGuest, GuestPromptTurn, GuestSession, PromptTurn, Session,
};
use acp_wasm_sys::layer::exports::yosh::acp::client::{
    Guest as ClientGuest, GuestTerminal,
};
use acp_wasm_sys::layer::yosh::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::layer::yosh::acp::errors::Error;
use acp_wasm_sys::layer::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use acp_wasm_sys::layer::yosh::acp::init::{
    AuthenticateRequest, InitializeRequest, InitializeResponse,
};
use acp_wasm_sys::layer::yosh::acp::prompts::{
    AvailableCommand, PromptResponse, SessionUpdate, StopReason,
};
use acp_wasm_sys::layer::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionId, SessionModeId, SessionModelId,
};
use acp_wasm_sys::layer::yosh::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};
use acp_wasm_sys::layer::yosh::acp::{agent, client};
use wit_bindgen::rt::async_support::StreamReader;

struct Layer;

/// Layer-side session resource. Wraps the downstream stage's owned
/// session handle so that dropping the upstream resource cascades the
/// close downstream.
pub struct LayerSession {
    /// The wire-level session id, used when emitting client-direction
    /// notifications from within session methods.
    session_id: String,
    /// Owned import-side resource handle for the downstream session.
    /// Used by [`GuestSession`] methods to forward to the next stage.
    downstream: agent::Session,
}

impl GuestSession for LayerSession {
    async fn prompt(&self, prompt: Vec<ContentBlock>) -> PromptTurn {
        // Phase 1: a `/shout` interception still ends the turn locally with
        // no streamed updates; everything else delegates to the downstream
        // session's prompt-turn. Phase 3 wires the layer-side stream `map`
        // for uppercasing agent-direction text on real turns.
        if is_shout_command(&prompt) {
            let now_on = !SHOUT_ENABLED.fetch_xor(true, Ordering::Relaxed);
            let msg = if now_on { "CAPS LOCK ENGAGED!" } else { "no more capsie lock :)" };
            return PromptTurn::new(LayerPromptTurn::ShoutAck {
                _session_id: self.session_id.clone(),
                msg: std::cell::RefCell::new(Some(msg.to_string())),
            });
        }
        let ds_turn = self.downstream.prompt(prompt).await;
        PromptTurn::new(LayerPromptTurn::Forward {
            _session_id: self.session_id.clone(),
            downstream: ds_turn,
        })
    }

    async fn set_mode(&self, mode_id: SessionModeId) -> Result<(), Error> {
        self.downstream.set_mode(mode_id).await
    }

    async fn select_model(&self, model_id: SessionModelId) -> Result<(), Error> {
        self.downstream.select_model(model_id).await
    }
}

/// Phase-1 prompt-turn stub. Either delegates to a downstream
/// prompt-turn (the common case) or, for the `/shout` intercept,
/// short-circuits with an empty stream + an immediate `EndTurn`
/// response. Phase 3 introduces a `Map` variant that wraps the
/// downstream stream and uppercases agent-direction text.
pub enum LayerPromptTurn {
    Forward {
        _session_id: String,
        downstream: agent::PromptTurn,
    },
    ShoutAck {
        _session_id: String,
        // Held so `response()` can be called once; subsequent calls
        // return an internal error rather than panicking.
        msg: std::cell::RefCell<Option<String>>,
    },
}

impl GuestPromptTurn for LayerPromptTurn {
    fn updates(&self) -> StreamReader<SessionUpdate> {
        match self {
            LayerPromptTurn::Forward { downstream, .. } => {
                // Phase 1: forward verbatim. Phase 3: map over this
                // stream to uppercase agent-direction text in flight.
                downstream.updates()
            }
            LayerPromptTurn::ShoutAck { .. } => {
                // Empty stream — nothing to deliver before the
                // response resolves. Phase 3 may switch this to a
                // one-shot stream containing the ack chunk.
                let (_w, r) = acp_wasm_sys::layer::wit_stream::new::<SessionUpdate>();
                r
            }
        }
    }

    async fn response(&self) -> Result<PromptResponse, Error> {
        match self {
            LayerPromptTurn::Forward { downstream, .. } => downstream.response().await,
            LayerPromptTurn::ShoutAck { msg, .. } => {
                // Consume the message so a duplicate call surfaces a
                // clear error rather than re-running the side effect.
                let _ = msg.borrow_mut().take();
                Ok(PromptResponse { stop_reason: StopReason::EndTurn })
            }
        }
    }
}

/// Whether agent-emitted text should be shouted (uppercased). Toggled
/// in-process via the `/shout` slash command; not persisted across
/// component restarts.
static SHOUT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Push the layer's `available-commands-update` upstream so the editor
/// learns about `/shout`. Sent after each session lifecycle method.
///
/// Phase 1: stubbed out (no `client::update_session` anymore). Phase 3
/// will push this onto the active prompt-turn's stream OR via a
/// dedicated commands channel — design TBD.
#[allow(dead_code)]
async fn advertise_commands(_session_id: &SessionId) {
    let _cmds = vec![AvailableCommand {
        name: "shout".to_string(),
        description: "Toggle uppercase rewriting of agent output for this session.".to_string(),
        input: None,
    }];
    // TODO(streams phase 3): emit this as a `SessionUpdate::AvailableCommandsUpdate`
    // on the next prompt-turn's stream, or carve out a separate
    // commands-advertisement channel on the session resource.
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
    type Session = LayerSession;
    type PromptTurn = LayerPromptTurn;

    async fn initialize(req: InitializeRequest) -> Result<InitializeResponse, Error> {
        agent::initialize(req).await
    }

    async fn authenticate(req: AuthenticateRequest) -> Result<(), Error> {
        agent::authenticate(req).await
    }

    async fn new_session(req: NewSessionRequest) -> Result<(Session, NewSessionResponse), Error> {
        let (ds_session, resp) = agent::new_session(req).await?;
        advertise_commands(&resp.session_id).await;
        let session = Session::new(LayerSession {
            session_id: resp.session_id.clone(),
            downstream: ds_session,
        });
        Ok((session, resp))
    }

    async fn load_session(
        req: LoadSessionRequest,
    ) -> Result<(Session, LoadSessionResponse), Error> {
        let sid = req.session_id.clone();
        let (ds_session, resp) = agent::load_session(req).await?;
        advertise_commands(&sid).await;
        let session = Session::new(LayerSession {
            session_id: sid,
            downstream: ds_session,
        });
        Ok((session, resp))
    }

    async fn list_sessions(req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        agent::list_sessions(req).await
    }

    async fn resume_session(
        req: ResumeSessionRequest,
    ) -> Result<(Session, ResumeSessionResponse), Error> {
        let sid = req.session_id.clone();
        let (ds_session, resp) = agent::resume_session(req).await?;
        advertise_commands(&sid).await;
        let session = Session::new(LayerSession {
            session_id: sid,
            downstream: ds_session,
        });
        Ok((session, resp))
    }
}

// -----------------------------------------------------------------------------
// client direction: forward to next stage. The legacy `update_session`
// path is gone — update-stream rewriting now happens in `LayerPromptTurn`
// (phase 3). Terminal lifecycle moved onto a `terminal` resource which
// this phase 1 stub does not yet implement.
// -----------------------------------------------------------------------------

pub struct LayerTerminal;

impl GuestTerminal for LayerTerminal {
    fn new(_req: acp_wasm_sys::layer::yosh::acp::terminals::CreateTerminalRequest) -> Self {
        unimplemented!("phase 2: LayerTerminal::new")
    }

    fn output(&self) -> StreamReader<u8> {
        // Phase 2 wires this through to the downstream/host terminal.
        let (_w, r) = acp_wasm_sys::layer::wit_stream::new::<u8>();
        r
    }

    async fn wait_for_exit(
        &self,
    ) -> Result<acp_wasm_sys::layer::yosh::acp::terminals::TerminalExitStatus, Error> {
        unimplemented!("phase 2: LayerTerminal::wait_for_exit")
    }
}

impl ClientGuest for Layer {
    type Terminal = LayerTerminal;

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
}

acp_wasm_sys::layer::export!(Layer with_types_in acp_wasm_sys::layer);
