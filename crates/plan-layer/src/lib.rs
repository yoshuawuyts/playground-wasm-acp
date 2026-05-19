//! ACP layer that adds a first-class "plan" session mode.
//!
//! Plan mode is exposed via the ACP session-mode mechanism (not a
//! slash command): the layer rewrites session lifecycle responses to
//! append a `plan` mode alongside whatever modes the downstream
//! provider advertises, intercepts `set-session-mode` to remember the
//! choice in-process, and enforces read-only semantics while plan
//! mode is active by:
//!
//! - Auto-denying `request-permission` for destructive tool kinds
//!   (`edit`, `delete`, `move`, `execute`, `other`) via a synthesized
//!   `reject-once` response. The agent never sees the user.
//! - Refusing `write-text-file` and `create-terminal` outright with
//!   an `invalid-request` error.
//!
//! Read paths (`read-text-file`, `get-terminal-output`, etc.) are
//! forwarded verbatim so the agent can still investigate the
//! codebase while drafting a plan.
//!
//! State is per-session, kept in a process-local map. Not persisted
//! across component restarts.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::sync::Mutex;

use acp_wasm_sys::layer::exports::yosh::acp::agent::Guest as AgentGuest;
use acp_wasm_sys::layer::exports::yosh::acp::client::Guest as ClientGuest;
use acp_wasm_sys::layer::yosh::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::layer::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use acp_wasm_sys::layer::yosh::acp::init::{
    AuthenticateRequest, InitializeRequest, InitializeResponse,
};
use acp_wasm_sys::layer::yosh::acp::prompts::{PromptRequest, PromptResponse, SessionUpdate};
use acp_wasm_sys::layer::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SessionMode, SessionModeState, SetSessionModeRequest,
};
use acp_wasm_sys::layer::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use acp_wasm_sys::layer::yosh::acp::tools::{
    PermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ToolKind,
};
use acp_wasm_sys::layer::yosh::acp::{agent, client};

struct Layer;

const PLAN_MODE_ID: &str = "plan";
const DEFAULT_MODE_ID: &str = "default";

/// Per-session record of whether plan mode is currently active. We
/// only store sessions we've seen; absence means "not plan".
static SESSION_PLAN: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut HashMap<String, bool>) -> R) -> R {
    let mut guard = SESSION_PLAN.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

fn set_plan(session: &SessionId, on: bool) {
    with_state(|m| {
        m.insert(session.to_string(), on);
    });
}

fn is_plan(session: &SessionId) -> bool {
    with_state(|m| m.get(session.as_str()).copied().unwrap_or(false))
}

/// Build the `plan` mode descriptor.
fn plan_mode() -> SessionMode {
    SessionMode {
        id: PLAN_MODE_ID.to_string(),
        name: "Plan".to_string(),
        description: Some(
            "Read-only research mode: agent investigates and proposes a plan but cannot edit \
             files or run commands."
                .to_string(),
        ),
    }
}

fn default_mode() -> SessionMode {
    SessionMode {
        id: DEFAULT_MODE_ID.to_string(),
        name: "Default".to_string(),
        description: Some("Normal execution: agent may edit files and run tools.".to_string()),
    }
}

/// Inject the `plan` mode into a downstream `modes` field. If the
/// downstream didn't advertise any modes, synthesize a minimal pair
/// (`default` + `plan`) and pick `default` as current.
fn inject_plan_mode(modes: Option<SessionModeState>) -> Option<SessionModeState> {
    match modes {
        Some(mut state) => {
            if !state
                .available_modes
                .iter()
                .any(|m| m.id == PLAN_MODE_ID)
            {
                state.available_modes.push(plan_mode());
            }
            Some(state)
        }
        None => Some(SessionModeState {
            current_mode_id: DEFAULT_MODE_ID.to_string(),
            available_modes: vec![default_mode(), plan_mode()],
        }),
    }
}

/// True for tool kinds that mutate state or run code. Used to gate
/// permission requests in plan mode.
fn is_destructive(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Execute | ToolKind::Other
    )
}

/// Synthesize a "reject this call" permission response. We try to
/// pick a reject-flavored option from the request; if none is
/// offered, we fall back to selecting the first option (best
/// effort — most clients always include a reject option).
fn synth_reject(req: &RequestPermissionRequest) -> RequestPermissionResponse {
    use acp_wasm_sys::layer::yosh::acp::tools::PermissionOptionKind;
    let chosen = req
        .options
        .iter()
        .find(|o| {
            matches!(
                o.kind,
                PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
            )
        })
        .or_else(|| req.options.first());
    let id = chosen.map(|o| o.id.clone()).unwrap_or_default();
    RequestPermissionResponse {
        outcome: PermissionOutcome::Selected(id),
    }
}

// -----------------------------------------------------------------------------
// agent direction: rewrite session lifecycle, intercept set-session-mode
// -----------------------------------------------------------------------------

impl AgentGuest for Layer {
    async fn initialize(req: InitializeRequest) -> Result<InitializeResponse, Error> {
        agent::initialize(req).await
    }

    async fn authenticate(req: AuthenticateRequest) -> Result<(), Error> {
        agent::authenticate(req).await
    }

    async fn new_session(req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let mut resp = agent::new_session(req).await?;
        resp.modes = inject_plan_mode(resp.modes);
        set_plan(&resp.session_id, false);
        Ok(resp)
    }

    async fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let sid = req.session_id.clone();
        let mut resp = agent::load_session(req).await?;
        resp.modes = inject_plan_mode(resp.modes);
        set_plan(&sid, false);
        Ok(resp)
    }

    async fn list_sessions(req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        agent::list_sessions(req).await
    }

    async fn resume_session(req: ResumeSessionRequest) -> Result<ResumeSessionResponse, Error> {
        let sid = req.session_id.clone();
        let mut resp = agent::resume_session(req).await?;
        resp.modes = inject_plan_mode(resp.modes);
        // Resume doesn't replay history; leave plan state as-is if we
        // already know about this session, else assume off.
        with_state(|m| {
            m.entry(sid.to_string()).or_insert(false);
        });
        Ok(resp)
    }

    async fn close_session(session_id: SessionId) -> Result<(), Error> {
        with_state(|m| {
            m.remove(session_id.as_str());
        });
        agent::close_session(session_id).await
    }

    async fn set_session_mode(req: SetSessionModeRequest) -> Result<(), Error> {
        // Modes we manage locally: short-circuit so the downstream
        // (which doesn't know about them) is never asked. Notify the
        // client of the new active mode.
        if req.mode_id == PLAN_MODE_ID {
            set_plan(&req.session_id, true);
            client::update_session(
                req.session_id.clone(),
                SessionUpdate::CurrentModeUpdate(PLAN_MODE_ID.to_string()),
            )
            .await;
            return Ok(());
        }
        if req.mode_id == DEFAULT_MODE_ID {
            set_plan(&req.session_id, false);
            client::update_session(
                req.session_id.clone(),
                SessionUpdate::CurrentModeUpdate(DEFAULT_MODE_ID.to_string()),
            )
            .await;
            return Ok(());
        }
        // Anything else: forward downstream (it's a mode the provider
        // advertised). Leaving plan mode off if the user moves to a
        // provider-managed mode keeps the semantics predictable.
        set_plan(&req.session_id, false);
        agent::set_session_mode(req).await
    }

    async fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
        agent::prompt(req).await
    }

    async fn cancel(session_id: SessionId) {
        agent::cancel(session_id).await;
    }
}

// -----------------------------------------------------------------------------
// client direction: enforce read-only semantics while plan mode is on
// -----------------------------------------------------------------------------

impl ClientGuest for Layer {
    async fn update_session(session_id: SessionId, update: SessionUpdate) {
        // Outbound updates pass through verbatim. Permission gating
        // at `request-permission` is the authoritative enforcement
        // point in plan mode, not session-update filtering.
        client::update_session(session_id, update).await;
    }

    async fn request_permission(
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        if is_plan(&req.session_id) {
            // The tool-call-update may or may not carry a kind. If
            // it's a destructive kind (or absent — be conservative),
            // reject without bothering the user.
            let kind_is_destructive = match req.tool_call.kind {
                Some(k) => is_destructive(k),
                None => true,
            };
            if kind_is_destructive {
                eprintln!(
                    "plan-layer: auto-rejecting destructive tool-call in plan mode (session={})",
                    req.session_id
                );
                return Ok(synth_reject(&req));
            }
        }
        client::request_permission(req).await
    }

    async fn read_text_file(req: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        client::read_text_file(req).await
    }

    async fn write_text_file(req: WriteTextFileRequest) -> Result<(), Error> {
        if is_plan(&req.session_id) {
            eprintln!(
                "plan-layer: blocking write-text-file in plan mode (session={}, path={})",
                req.session_id, req.path
            );
            return Err(Error {
                code: ErrorCode::InvalidRequest,
                message: "Plan mode is active: file writes are not permitted. Switch out of \
                          plan mode to apply changes."
                    .to_string(),
            });
        }
        client::write_text_file(req).await
    }

    async fn create_terminal(req: CreateTerminalRequest) -> Result<CreateTerminalResponse, Error> {
        if is_plan(&req.session_id) {
            eprintln!(
                "plan-layer: blocking create-terminal in plan mode (session={}, cmd={})",
                req.session_id, req.command
            );
            return Err(Error {
                code: ErrorCode::InvalidRequest,
                message: "Plan mode is active: command execution is not permitted. Switch out \
                          of plan mode to run commands."
                    .to_string(),
            });
        }
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

    async fn release_terminal(session_id: SessionId, terminal_id: TerminalId) -> Result<(), Error> {
        client::release_terminal(session_id, terminal_id).await
    }
}

acp_wasm_sys::layer::export!(Layer with_types_in acp_wasm_sys::layer);
