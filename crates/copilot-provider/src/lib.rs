//! ACP wasm provider that forwards prompts to the GitHub Copilot chat API.
//!
//! Mirrors `ollama-provider` but targets Copilot: it resolves a GitHub token
//! from the host secrets store (or env), exchanges it for a short-lived
//! Copilot API token, and streams OpenAI-compatible chat completions back to
//! the editor as `session/update` notifications. Text only — no tool calls.

mod copilot;
mod storage;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::provider::exports::yosh::acp::agent::{
    Guest, GuestPromptTurn, GuestSession, PromptTurn, Session,
};
use acp_wasm_sys::provider::yosh::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::provider::yosh::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::provider::yosh::acp::init::{
    AgentCapabilities, AuthenticateRequest, ImplementationInfo, InitializeRequest,
    InitializeResponse, McpCapabilities, PromptCapabilities, SessionCapabilities,
};
use acp_wasm_sys::provider::yosh::acp::prompts::{PromptResponse, SessionUpdate, StopReason};
use acp_wasm_sys::provider::yosh::acp::sessions::{
    ComponentSource, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionModeId, SessionModel, SessionModelId, SessionModelState,
};

use crate::copilot::Message;
use crate::storage::SessionState;

struct Agent;

/// Host-side representation of an `agent.session` resource. Dropping it evicts
/// the session's in-memory state.
pub struct ProviderSession {
    id: String,
}

impl Drop for ProviderSession {
    fn drop(&mut self) {
        SESSIONS.with(|s| {
            s.borrow_mut().remove(&self.id);
        });
    }
}

impl GuestSession for ProviderSession {
    async fn prompt(&self, prompt: Vec<ContentBlock>) -> PromptTurn {
        // Allocate the body stream up front. Both halves go into the returned
        // prompt-turn resource; `response()` parks the writer in
        // `ACTIVE_WRITER` while running the prompt loop so `emit_update` finds
        // it, then drops it to EOF the stream.
        let (writer, reader) = acp_wasm_sys::provider::wit_stream::new::<SessionUpdate>();
        PromptTurn::new(ProviderPromptTurn {
            session_id: self.id.clone(),
            inputs: RefCell::new(Some(prompt)),
            writer: RefCell::new(Some(writer)),
            reader: RefCell::new(Some(reader)),
        })
    }

    async fn set_mode(&self, _mode_id: SessionModeId) -> Result<(), Error> {
        // Provider advertises no modes; the slot is reserved for layers.
        Err(err(
            ErrorCode::InvalidParams,
            "copilot provider does not advertise any modes",
        ))
    }

    async fn select_model(&self, model_id: SessionModelId) -> Result<(), Error> {
        let session_id = self.id.clone();
        SESSIONS
            .with(|s| {
                let mut sessions = s.borrow_mut();
                match sessions.get_mut(&session_id) {
                    Some(session) => {
                        session.model = model_id;
                        Ok(())
                    }
                    None => Err(format!("unknown session id: {session_id}")),
                }
            })
            .map_err(|e| err(ErrorCode::InvalidParams, &e))
    }
}

/// Owned state for a `prompt-turn` resource. Constructed by
/// [`GuestSession::prompt`]; consumed by either [`updates()`] (which hands out
/// the reader) or [`response()`] (which runs the prompt loop while writing
/// updates into the writer).
pub struct ProviderPromptTurn {
    session_id: String,
    inputs: RefCell<Option<Vec<ContentBlock>>>,
    writer: RefCell<Option<wit_bindgen::rt::async_support::StreamWriter<SessionUpdate>>>,
    reader: RefCell<Option<wit_bindgen::rt::async_support::StreamReader<SessionUpdate>>>,
}

// Single-threaded wasm guest: a thread-local cell is enough.
thread_local! {
    static ACTIVE_WRITER: RefCell<
        Option<wit_bindgen::rt::async_support::StreamWriter<SessionUpdate>>,
    > = const { RefCell::new(None) };
}

impl GuestPromptTurn for ProviderPromptTurn {
    async fn updates(&self) -> wit_bindgen::rt::async_support::StreamReader<SessionUpdate> {
        // First call wins. Subsequent calls get an immediately-EOF empty
        // stream.
        match self.reader.borrow_mut().take() {
            Some(r) => r,
            None => {
                let (_w, r) = acp_wasm_sys::provider::wit_stream::new::<SessionUpdate>();
                r
            }
        }
    }

    async fn response(&self) -> Result<PromptResponse, Error> {
        let inputs = match self.inputs.borrow_mut().take() {
            Some(v) => v,
            None => {
                return Err(err(
                    ErrorCode::InternalError,
                    "prompt-turn.response called twice",
                ));
            }
        };
        let writer = self.writer.borrow_mut().take();
        ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = writer);
        let r = prompt_impl(self.session_id.clone(), inputs).await;
        // Drop the writer so the host-side reader sees end-of-stream.
        ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = None);
        r
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Push a `SessionUpdate` onto the currently-active prompt-turn's body stream.
/// No-op if nothing is active.
async fn emit_update(_session_id: String, update: SessionUpdate) {
    let Some(mut writer) = ACTIVE_WRITER.with(|cell| cell.borrow_mut().take()) else {
        return;
    };
    let (result, _buf) = writer.write(vec![update]).await;
    // If the host closed the reader, drop the writer instead of writing again
    // (which would trap). Skip re-parking so future emits no-op.
    use wit_bindgen::rt::async_support::StreamResult;
    if matches!(result, StreamResult::Dropped | StreamResult::Cancelled) {
        return;
    }
    ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = Some(writer));
}

// Wasm components are single-threaded; thread-local + RefCell avoids any
// synchronization cost while keeping interior mutability.
thread_local! {
    static SESSIONS: RefCell<HashMap<String, SessionState>> = RefCell::new(HashMap::new());
}

/// One-time system message inserted at the start of every fresh session.
const SYSTEM_PROMPT: &str = "You are GitHub Copilot, an AI coding assistant connected to the user's editor. Answer concisely and helpfully. When you include code, use fenced code blocks tagged with the language.";

/// Build the [`SessionModelState`] from a listing of chat-capable models.
///
/// The current model defaults to `preferred_model` when present in the list,
/// otherwise [`copilot::default_model`], otherwise the first model listed.
fn build_models_state_from_listed(
    listed: Vec<copilot::CopilotModel>,
    preferred_model: Option<&str>,
) -> (SessionModelState, String) {
    let default = copilot::default_model();

    let pick = |want: &str| listed.iter().any(|m| m.id == want);
    let current = preferred_model
        .filter(|m| pick(m))
        .map(|s| s.to_string())
        .or_else(|| if pick(&default) { Some(default.clone()) } else { None })
        .unwrap_or_else(|| listed[0].id.clone());

    let available_models = listed
        .into_iter()
        .map(|m| SessionModel {
            id: m.id,
            name: m.name,
            description: None,
            provided_by: ComponentSource {
                component_id: "local:copilot-provider".to_string(),
            },
        })
        .collect();

    (
        SessionModelState {
            current_model_id: current.clone(),
            available_models,
        },
        current,
    )
}

/// List chat-capable Copilot models for the ACP model picker. Falls back to a
/// single entry for [`copilot::default_model`] if the API is unreachable or
/// returns nothing, so session creation still succeeds; the failure is logged.
async fn build_models_state(preferred_model: Option<&str>) -> (SessionModelState, String) {
    let default = copilot::default_model();
    let listed = match copilot::list_models().await {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            eprintln!("copilot returned no models; using default");
            vec![copilot::CopilotModel {
                id: default.clone(),
                name: default.clone(),
            }]
        }
        Err(e) => {
            eprintln!("failed to list copilot models ({e}); using default");
            vec![copilot::CopilotModel {
                id: default.clone(),
                name: default.clone(),
            }]
        }
    };
    build_models_state_from_listed(listed, preferred_model)
}

fn next_session_id() -> String {
    // Mix wall-clock seconds with a per-process counter so ids are unique
    // across host restarts.
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("copilot-session-{secs:x}-{n}")
}

fn err(code: ErrorCode, message: &str) -> Error {
    Error {
        code,
        message: message.to_string(),
    }
}

/// Pull all `text` content blocks from a prompt, joined with blank lines.
fn extract_user_text(prompt: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in prompt {
        if let ContentBlock::Text(TextContent { text }) = block {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
    }
    out
}

impl Guest for Agent {
    type Session = ProviderSession;
    type PromptTurn = ProviderPromptTurn;

    async fn initialize(_req: InitializeRequest) -> Result<InitializeResponse, Error> {
        Ok(InitializeResponse {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                },
                mcp_capabilities: McpCapabilities {
                    http: false,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: false,
                    resume: true,
                    close: false,
                },
            },
            agent_info: Some(ImplementationInfo {
                name: "copilot-wasm-agent".to_string(),
                title: Some("GitHub Copilot (wasm)".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            auth_methods: Vec::new(),
        })
    }

    async fn authenticate(_req: AuthenticateRequest) -> Result<(), Error> {
        // Auth is handled out-of-band via the host secrets store / env; there
        // is no interactive auth method to run here.
        Err(err(
            ErrorCode::MethodNotFound,
            "authentication not required; configure a GitHub token via the host secrets store",
        ))
    }

    async fn new_session(req: NewSessionRequest) -> Result<(Session, NewSessionResponse), Error> {
        let id = next_session_id();
        let (models, current_model) = build_models_state(None).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                SessionState {
                    history: Vec::new(),
                    model: current_model,
                    cwd: req.cwd,
                },
            )
        });
        let resource = Session::new(ProviderSession { id: id.clone() });
        Ok((
            resource,
            NewSessionResponse {
                session_id: id,
                modes: None,
                models: Some(models),
            },
        ))
    }

    async fn load_session(
        req: LoadSessionRequest,
    ) -> Result<(Session, LoadSessionResponse), Error> {
        let session_id = req.session_id.clone();
        // Load the persisted session if present, then replay history to the
        // client as `update-session` notifications (per the ACP spec for
        // `session/load`). Missing file = fresh session.
        let default = copilot::default_model();
        let stored = match storage::load(&session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("load: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let (models, current_model) = build_models_state(preferred.as_deref()).await;
        for msg in &history {
            let block = ContentBlock::Text(TextContent {
                text: msg.content.clone(),
            });
            let update = match msg.role.as_str() {
                "user" => SessionUpdate::UserMessageChunk(block),
                "assistant" => SessionUpdate::AgentMessageChunk(block),
                _ => continue,
            };
            emit_update(session_id.clone(), update).await;
        }
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                session_id.clone(),
                SessionState {
                    history,
                    model: current_model,
                    cwd: req.cwd,
                },
            );
        });
        let resource = Session::new(ProviderSession { id: session_id });
        Ok((
            resource,
            LoadSessionResponse {
                modes: None,
                models: Some(models),
            },
        ))
    }

    async fn list_sessions(_req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        Err(err(ErrorCode::MethodNotFound, "list-sessions not supported"))
    }

    async fn resume_session(
        req: ResumeSessionRequest,
    ) -> Result<(Session, ResumeSessionResponse), Error> {
        let session_id = req.session_id.clone();
        // Like `load_session`, but the spec forbids replaying history via
        // `update-session`. Just rehydrate the in-memory map.
        let default = copilot::default_model();
        let stored = match storage::load(&session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("resume: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let (models, current_model) = build_models_state(preferred.as_deref()).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                session_id.clone(),
                SessionState {
                    history,
                    model: current_model,
                    cwd: req.cwd,
                },
            );
        });
        let resource = Session::new(ProviderSession { id: session_id });
        Ok((
            resource,
            ResumeSessionResponse {
                modes: None,
                models: Some(models),
            },
        ))
    }
}

async fn prompt_impl(
    session_id: String,
    prompt: Vec<ContentBlock>,
) -> Result<PromptResponse, Error> {
    let user_text = extract_user_text(&prompt);
    if user_text.is_empty() {
        return Err(err(
            ErrorCode::InvalidParams,
            "prompt contained no text content",
        ));
    }

    // Append the user turn to this session's history and grab a copy (plus the
    // active model) to send to Copilot. New sessions can land here without
    // going through `new-session` (e.g. tests); fall back to the default model.
    // A one-time `system` message is prepended on the first prompt.
    let (history, model) = SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        let entry = sessions.entry(session_id.clone()).or_insert_with(|| SessionState {
            history: Vec::new(),
            model: copilot::default_model(),
            cwd: String::new(),
        });
        if entry.history.is_empty() {
            let mut prompt = SYSTEM_PROMPT.to_string();
            if !entry.cwd.is_empty() {
                prompt.push_str("\n\nThe user's current project directory is `");
                prompt.push_str(&entry.cwd);
                prompt.push_str("`. Resolve relative file references against it.");
            }
            entry.history.push(Message::system(prompt));
        }
        entry.history.push(Message::user(user_text.clone()));
        (entry.history.clone(), entry.model.clone())
    });

    let session_id_chunk = session_id.clone();
    let reply = copilot::chat(model, history, |chunk| {
        let sid = session_id_chunk.clone();
        async move {
            emit_update(
                sid,
                SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent { text: chunk })),
            )
            .await;
        }
    })
    .await
    .map_err(|e| err(ErrorCode::InternalError, &format!("copilot: {e}")))?;

    // Append the assistant reply to history and persist.
    let snapshot = SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        sessions.get_mut(&session_id).map(|entry| {
            entry.history.push(Message::assistant(reply.clone()));
            entry.clone()
        })
    });
    if let Some(session) = snapshot {
        if let Err(e) = storage::save(&session_id, &session) {
            // Persistence is best-effort: a failed save shouldn't fail the
            // prompt turn. Surface it as a thought chunk.
            emit_update(
                session_id.clone(),
                SessionUpdate::AgentThoughtChunk(ContentBlock::Text(TextContent {
                    text: format!("(failed to persist session: {e})"),
                })),
            )
            .await;
        }
    }

    Ok(PromptResponse {
        stop_reason: StopReason::EndTurn,
    })
}

acp_wasm_sys::provider::export!(Agent with_types_in acp_wasm_sys::provider);
