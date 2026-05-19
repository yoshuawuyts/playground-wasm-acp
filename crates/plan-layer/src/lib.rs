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
    ComponentSource, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SelectModelRequest, SessionId, SessionMode, SessionModeState,
    SetSessionModeRequest,
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

/// Id of the host-injected default mode. The host owns this mode
/// and ensures it's always present; layers like this one don't
/// synthesize their own "not me" mode. When the user switches to
/// it, we flip plan off and short-circuit so the request isn't
/// forwarded to the (mode-less) downstream provider.
const HOST_DEFAULT_MODE_ID: &str = "default";

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
        provided_by: ComponentSource {
            component_id: "local:plan-layer".to_string(),
        },
    }
}

/// Inject `plan` into the downstream `modes` state, leaving the
/// notion of "not plan" to whoever else is contributing modes
/// (typically the host's synthetic `default`). If the downstream
/// stage advertised no modes at all, we still only contribute
/// `plan` — the host is responsible for ensuring there's a
/// non-plan mode available for the user to toggle back to.
fn inject_plan_mode(modes: Option<SessionModeState>) -> Option<SessionModeState> {
    let mut state = modes.unwrap_or(SessionModeState {
        // Empty placeholder; the host will fill in a current id when
        // it appends its `default` mode. Use the plan id as a
        // last-resort fallback so the field is never literally empty.
        current_mode_id: PLAN_MODE_ID.to_string(),
        available_modes: Vec::new(),
    });
    if !state.available_modes.iter().any(|m| m.id == PLAN_MODE_ID) {
        state.available_modes.push(plan_mode());
    }
    Some(state)
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
        eprintln!(
            "plan-layer: new_session downstream returned modes={:?}",
            resp.modes.as_ref().map(|s| s.available_modes.len())
        );
        resp.modes = inject_plan_mode(resp.modes);
        eprintln!(
            "plan-layer: new_session injecting; total modes={} session={}",
            resp.modes
                .as_ref()
                .map(|s| s.available_modes.len())
                .unwrap_or(0),
            resp.session_id,
        );
        set_plan(&resp.session_id, false);
        Ok(resp)
    }

    async fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let sid = req.session_id.clone();
        let mut resp = agent::load_session(req).await?;
        eprintln!(
            "plan-layer: load_session downstream returned modes={:?} session={}",
            resp.modes.as_ref().map(|s| s.available_modes.len()),
            sid,
        );
        resp.modes = inject_plan_mode(resp.modes);
        eprintln!(
            "plan-layer: load_session injecting; total modes={}",
            resp.modes
                .as_ref()
                .map(|s| s.available_modes.len())
                .unwrap_or(0),
        );
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
        // `plan` is ours: toggle plan-mode state and notify the
        // client. The host-injected `default` is the canonical
        // "disengage plan" id; recognise it here and short-circuit
        // so the request never reaches the provider (which doesn't
        // advertise any modes). Anything else: turn plan off and
        // forward downstream.
        if req.mode_id == PLAN_MODE_ID {
            set_plan(&req.session_id, true);
            client::update_session(
                req.session_id.clone(),
                SessionUpdate::CurrentModeUpdate(PLAN_MODE_ID.to_string()),
            )
            .await;
            return Ok(());
        }
        if req.mode_id == HOST_DEFAULT_MODE_ID {
            set_plan(&req.session_id, false);
            client::update_session(
                req.session_id.clone(),
                SessionUpdate::CurrentModeUpdate(HOST_DEFAULT_MODE_ID.to_string()),
            )
            .await;
            return Ok(());
        }
        set_plan(&req.session_id, false);
        agent::set_session_mode(req).await
    }

    async fn select_model(req: SelectModelRequest) -> Result<(), Error> {
        // Models are entirely the provider's concern; forward verbatim.
        agent::select_model(req).await
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
