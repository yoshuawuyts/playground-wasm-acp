//! ACP wasm guest agent that forwards prompts to a local Ollama server.

mod ollama;
mod storage;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::exports::yoshuawuyts::acp::agent::Guest;
use acp_wasm_sys::yoshuawuyts::acp::client;
use acp_wasm_sys::yoshuawuyts::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::yoshuawuyts::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::yoshuawuyts::acp::init::{
    AgentCapabilities, AuthenticateRequest, ImplementationInfo, InitializeRequest,
    InitializeResponse, McpCapabilities, PromptCapabilities, SessionCapabilities,
};
use acp_wasm_sys::yoshuawuyts::acp::prompts::{
    PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
use acp_wasm_sys::yoshuawuyts::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse, SessionId,
    SessionMode, SessionModeState, SetSessionModeRequest,
};

use crate::ollama::Message;
use crate::storage::Session;

struct Agent;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

// Wasm components are single-threaded; using thread-local + RefCell avoids
// any synchronization cost while keeping interior mutability.
thread_local! {
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
}

/// Build the [`SessionModeState`] for a freshly created or loaded session.
///
/// Lists the locally installed Ollama models and exposes them as session
/// modes (one mode per model). The current mode defaults to
/// `preferred_model` when present in the list, otherwise `default_model()`,
/// otherwise the first model returned by Ollama.
///
/// If Ollama is unreachable or returns no models, falls back to a single
/// mode for `default_model()` so session creation still succeeds; the
/// failure is logged to stderr.
fn build_modes_state(preferred_model: Option<&str>) -> (SessionModeState, String) {
    let default = ollama::default_model();
    let listed = match wstd::runtime::block_on(ollama::list_models()) {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            eprintln!("ollama returned no models; using default");
            vec![default.clone()]
        }
        Err(e) => {
            eprintln!("failed to list ollama models ({e}); using default");
            vec![default.clone()]
        }
    };

    // Pick the current mode: prefer the caller's hint, then OLLAMA_MODEL,
    // then the first available.
    let pick = |want: &str| listed.iter().any(|m| m == want);
    let current = preferred_model
        .filter(|m| pick(m))
        .map(|s| s.to_string())
        .or_else(|| {
            if pick(&default) {
                Some(default.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| listed[0].clone());

    let available_modes = listed
        .into_iter()
        .map(|name| SessionMode {
            id: name.clone(),
            name,
            description: None,
        })
        .collect();

    (
        SessionModeState {
            current_mode_id: current.clone(),
            available_modes,
        },
        current,
    )
}

fn next_session_id() -> String {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ollama-session-{n}")
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
    fn initialize(_req: InitializeRequest) -> Result<InitializeResponse, Error> {
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
                name: "ollama-wasm-agent".to_string(),
                title: Some("Ollama (wasm)".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            auth_methods: Vec::new(),
        })
    }

    fn authenticate(_req: AuthenticateRequest) -> Result<(), Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "authentication not required",
        ))
    }

    fn new_session(_req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let id = next_session_id();
        let (modes, current_model) = build_modes_state(None);
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                Session {
                    history: Vec::new(),
                    model: current_model,
                },
            )
        });
        Ok(NewSessionResponse {
            session_id: id,
            modes: Some(modes),
        })
    }

    fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        // Load the persisted session if present, then replay history to
        // the client as `update-session` notifications (per the ACP spec
        // for `session/load`). Missing file = fresh session.
        let default = ollama::default_model();
        let stored = match storage::load(&req.session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("load: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let (modes, current_model) = build_modes_state(preferred.as_deref());
        for msg in &history {
            let block = ContentBlock::Text(TextContent {
                text: msg.content.clone(),
            });
            let update = match msg.role.as_str() {
                "user" => SessionUpdate::UserMessageChunk(block),
                "assistant" => SessionUpdate::AgentMessageChunk(block),
                _ => continue,
            };
            client::update_session(&req.session_id, &update);
        }
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                req.session_id.clone(),
                Session {
                    history,
                    model: current_model,
                },
            );
        });
        Ok(LoadSessionResponse { modes: Some(modes) })
    }

    fn list_sessions(_req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "list-sessions not supported",
        ))
    }

    fn resume_session(req: ResumeSessionRequest) -> Result<ResumeSessionResponse, Error> {
        // Like `load_session`, but the spec forbids replaying history
        // via `update-session`. Just rehydrate the in-memory map.
        let default = ollama::default_model();
        let stored = match storage::load(&req.session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("resume: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let (modes, current_model) = build_modes_state(preferred.as_deref());
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                req.session_id,
                Session {
                    history,
                    model: current_model,
                },
            );
        });
        Ok(ResumeSessionResponse { modes: Some(modes) })
    }

    fn close_session(_session_id: SessionId) -> Result<(), Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "close-session not supported",
        ))
    }

    fn set_session_mode(req: SetSessionModeRequest) -> Result<(), Error> {
        let SetSessionModeRequest {
            session_id,
            mode_id,
        } = req;
        let switched = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            match sessions.get_mut(&session_id) {
                Some(session) => {
                    session.model = mode_id.clone();
                    Ok(())
                }
                None => Err(format!("unknown session id: {session_id}")),
            }
        });
        switched.map_err(|e| err(ErrorCode::InvalidParams, &e))?;
        // Notify the client that the active mode changed. Per the ACP
        // spec, the agent SHOULD emit `current-mode-update` so the editor
        // can reflect the new selection in its picker.
        client::update_session(&session_id, &SessionUpdate::CurrentModeUpdate(mode_id));
        Ok(())
    }

    fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
        let user_text = extract_user_text(&req.prompt);
        if user_text.is_empty() {
            return Err(err(
                ErrorCode::InvalidParams,
                "prompt contained no text content",
            ));
        }

        // Append the user turn to this session's history and grab a copy
        // (plus the active model) to send to Ollama. New sessions can land
        // here without going through `new-session` (e.g. tests); fall back
        // to the default model in that case.
        let (history, model) = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            let entry = sessions
                .entry(req.session_id.clone())
                .or_insert_with(|| Session {
                    history: Vec::new(),
                    model: ollama::default_model(),
                });
            entry.history.push(Message {
                role: "user".to_string(),
                content: user_text.clone(),
            });
            (entry.history.clone(), entry.model.clone())
        });

        let session_id = req.session_id.clone();
        let assistant = wstd::runtime::block_on(ollama::chat(&model, &history, |chunk| {
            client::update_session(
                &session_id,
                &SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent {
                    text: chunk.to_string(),
                })),
            );
        }))
        .map_err(|e| err(ErrorCode::InternalError, &format!("ollama: {e}")))?;

        // Record the assistant's reply for the next turn, then persist
        // the updated session so the next instance can `load`/`resume`.
        let snapshot = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            sessions.get_mut(&req.session_id).map(|entry| {
                entry.history.push(Message {
                    role: "assistant".to_string(),
                    content: assistant,
                });
                entry.clone()
            })
        });
        if let Some(session) = snapshot {
            if let Err(e) = storage::save(&req.session_id, &session) {
                // Persistence is best-effort: a failed save shouldn't fail
                // the prompt turn (the user already saw the reply). Log
                // via a thought chunk so it's visible.
                client::update_session(
                    &req.session_id,
                    &SessionUpdate::AgentThoughtChunk(ContentBlock::Text(TextContent {
                        text: format!("(failed to persist session: {e})"),
                    })),
                );
            }
        }

        Ok(PromptResponse {
            stop_reason: StopReason::EndTurn,
        })
    }

    fn cancel(_session_id: SessionId) {
        // TODO: real cancellation. The host serializes all wasm calls behind
        // a single mutex, so `cancel` cannot run while `prompt` is in
        // flight. Implementing proper cancellation requires moving the
        // streaming loop off the lock and using a shared cancellation flag.
    }
}

acp_wasm_sys::export!(Agent with_types_in acp_wasm_sys);
