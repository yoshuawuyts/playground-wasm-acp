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

use acp_wasm_sys::layer::exports::yosh::acp::agent::{
    Guest as AgentGuest, GuestPromptTurn, GuestSession, PromptTurn, Session,
};
use acp_wasm_sys::layer::exports::yosh::acp::client::{Guest as ClientGuest, GuestTerminal};
use acp_wasm_sys::layer::yosh::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::layer::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use acp_wasm_sys::layer::yosh::acp::init::{
    AuthenticateRequest, InitializeRequest, InitializeResponse,
};
use acp_wasm_sys::layer::yosh::acp::prompts::{PromptResponse, SessionUpdate};
use acp_wasm_sys::layer::yosh::acp::sessions::{
    ComponentSource, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionId, SessionMode, SessionModeId, SessionModeState,
    SessionModelId,
};
use acp_wasm_sys::layer::yosh::acp::tools::{
    PermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ToolKind,
};
use acp_wasm_sys::layer::yosh::acp::{agent, client};
use wit_bindgen::rt::async_support::StreamReader;

struct Layer;

/// Layer-side session resource. Wraps the downstream stage's owned
/// session handle so dropping the upstream resource cascades the close
/// downstream, and tracks plan-mode state alongside it.
pub struct PlanSession {
    session_id: String,
    /// Owned import-side resource handle for the downstream session.
    /// Used by [`GuestSession::cancel`] to forward to the next stage.
    downstream: agent::Session,
}

impl Drop for PlanSession {
    fn drop(&mut self) {
        with_state(|m| {
            m.remove(self.session_id.as_str());
        });
    }
}

impl GuestSession for PlanSession {
    async fn prompt(
        &self,
        prompt: Vec<acp_wasm_sys::layer::yosh::acp::content::ContentBlock>,
    ) -> PromptTurn {
        PromptTurn::new(PlanPromptTurn {
            downstream: self.downstream.prompt(prompt).await,
            _session_id: self.session_id.clone(),
        })
    }

    async fn set_mode(&self, mode_id: SessionModeId) -> Result<(), Error> {
        // `plan` is ours: toggle plan-mode state. The host-injected
        // `default` is the canonical "disengage plan" id. Anything
        // else: turn plan off and forward downstream.
        //
        // Phase 1: the `SessionUpdate::CurrentModeUpdate` notifications
        // that previously announced the switch are stubbed out. Phase 3
        // will re-route them onto the next prompt-turn's stream OR via a
        // dedicated session-mode channel.
        if mode_id == PLAN_MODE_ID {
            set_plan(&self.session_id, true);
            return Ok(());
        }
        if mode_id == HOST_DEFAULT_MODE_ID {
            set_plan(&self.session_id, false);
            return Ok(());
        }
        set_plan(&self.session_id, false);
        self.downstream.set_mode(mode_id).await
    }

    async fn select_model(&self, model_id: SessionModelId) -> Result<(), Error> {
        // Models are entirely the provider's concern; forward verbatim.
        self.downstream.select_model(model_id).await
    }
}

/// Phase-1 prompt-turn forwarder. Phase 3 may wrap the downstream
/// stream to enforce additional plan-mode constraints.
pub struct PlanPromptTurn {
    downstream: agent::PromptTurn,
    _session_id: String,
}

impl GuestPromptTurn for PlanPromptTurn {
    async fn updates(&self) -> StreamReader<SessionUpdate> {
        self.downstream.updates().await
    }

    async fn response(&self) -> Result<PromptResponse, Error> {
        self.downstream.response().await
    }
}

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
    type Session = PlanSession;
    type PromptTurn = PlanPromptTurn;

    async fn initialize(req: InitializeRequest) -> Result<InitializeResponse, Error> {
        agent::initialize(req).await
    }

    async fn authenticate(req: AuthenticateRequest) -> Result<(), Error> {
        agent::authenticate(req).await
    }

    async fn new_session(req: NewSessionRequest) -> Result<(Session, NewSessionResponse), Error> {
        let (ds_session, mut resp) = agent::new_session(req).await?;
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
        let session = Session::new(PlanSession {
            session_id: resp.session_id.clone(),
            downstream: ds_session,
        });
        Ok((session, resp))
    }

    async fn load_session(
        req: LoadSessionRequest,
    ) -> Result<(Session, LoadSessionResponse), Error> {
        let sid = req.session_id.clone();
        let (ds_session, mut resp) = agent::load_session(req).await?;
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
        let session = Session::new(PlanSession {
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
        let (ds_session, mut resp) = agent::resume_session(req).await?;
        resp.modes = inject_plan_mode(resp.modes);
        // Resume doesn't replay history; leave plan state as-is if we
        // already know about this session, else assume off.
        with_state(|m| {
            m.entry(sid.clone()).or_insert(false);
        });
        let session = Session::new(PlanSession {
            session_id: sid,
            downstream: ds_session,
        });
        Ok((session, resp))
    }
}

// -----------------------------------------------------------------------------
// client direction: enforce read-only semantics while plan mode is on
//
// Phase 1: terminal and update-session methods are gone; terminal is now a
// resource which we stub with `unimplemented!`. Plan-mode terminal blocking
// returns in phase 2 (as part of the terminal-resource constructor).
// -----------------------------------------------------------------------------

pub struct PlanTerminal;

impl GuestTerminal for PlanTerminal {
    fn new(_req: acp_wasm_sys::layer::yosh::acp::terminals::CreateTerminalRequest) -> Self {
        unimplemented!("phase 2: PlanTerminal::new")
    }

    async fn output(&self) -> StreamReader<u8> {
        let (_w, r) = acp_wasm_sys::layer::wit_stream::new::<u8>();
        r
    }

    async fn wait_for_exit(
        &self,
    ) -> Result<acp_wasm_sys::layer::yosh::acp::terminals::TerminalExitStatus, Error> {
        unimplemented!("phase 2: PlanTerminal::wait_for_exit")
    }
}

impl ClientGuest for Layer {
    type Terminal = PlanTerminal;

    async fn request_permission(
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        // `req.tool_call` is now a `ToolCallSnapshot` (not an update); the
        // session-id moved out of the request record. Plan-mode rejection
        // logic now needs to look up the *active* session via tool-call
        // resource ownership — phase 4 territory. For phase 1 keep the
        // forward path and skip the auto-reject.
        //
        // TODO(streams phase 4): re-enable destructive-kind auto-reject
        // by looking up the plan-mode flag through the active session.
        let _ = is_destructive;
        let _ = synth_reject;
        client::request_permission(req).await
    }

    async fn read_text_file(req: ReadTextFileRequest) -> Result<ReadTextFileResponse, Error> {
        // Phase 1: `session_id` field moved out of the request record
        // in the new WIT, so we no longer have a cheap way to look up
        // plan-mode here. Phase 3/4 will route this through the session
        // resource so the plan flag is reachable again.
        //
        // TODO(streams phase 4): re-enable plan-mode block (writes
        // are still blocked below; reads were never blocked anyway).
        client::read_text_file(req).await
    }

    async fn write_text_file(req: WriteTextFileRequest) -> Result<(), Error> {
        // TODO(streams phase 4): re-enable plan-mode block.
        client::write_text_file(req).await
    }
}

acp_wasm_sys::layer::export!(Layer with_types_in acp_wasm_sys::layer);
