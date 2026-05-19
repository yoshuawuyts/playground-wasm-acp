//! ACP wasm provider that forwards prompts to a local Ollama server.

mod ollama;
mod storage;
mod tools;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::provider::exports::yosh::acp::agent::Guest;
use acp_wasm_sys::provider::yosh::acp::client;
use acp_wasm_sys::provider::yosh::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::provider::yosh::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::provider::yosh::acp::init::{
    AgentCapabilities, AuthenticateRequest, ImplementationInfo, InitializeRequest,
    InitializeResponse, McpCapabilities, PromptCapabilities, SessionCapabilities,
};
use acp_wasm_sys::provider::yosh::acp::prompts::{
    PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
use acp_wasm_sys::provider::yosh::acp::sessions::{
    ComponentSource, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SelectModelRequest, SessionId, SessionModel, SessionModelState,
    SetSessionModeRequest,
};
use acp_wasm_sys::provider::yosh::acp::tools::ToolKind;

use crate::ollama::Message;
use crate::storage::Session;

struct Agent;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

// Wasm components are single-threaded; using thread-local + RefCell avoids
// any synchronization cost while keeping interior mutability.
thread_local! {
    static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
}

/// One-time system message inserted at the start of every fresh session.
///
/// Small tool-capable Ollama models will happily call any tool we
/// advertise on the slimmest pretext, including bare greetings. The
/// guidance below tells them to be conservative; it doesn't have to be
/// perfectly obeyed (we also defensively validate args server-side), but
/// it dramatically reduces useless tool-call storms.
const SYSTEM_PROMPT: &str = "You are a coding assistant connected to the user's editor. You have access to a `read_file` tool that can read source files in the user's project. Only call tools when the user explicitly asks you to read a file, or when reading a specific file is strictly necessary to answer the user's request. For greetings, small talk, or general questions, respond in plain text without calling any tools. Never call `read_file` with an empty path, `/`, `.`, or any directory.";

/// Build the [`SessionModelState`] for a freshly created or loaded session.
///
/// Lists the locally installed Ollama models and exposes them via
/// ACP's (unstable) `session/models` capability. The current model
/// defaults to `preferred_model` when present in the list, otherwise
/// `default_model()`, otherwise the first model returned by Ollama.
///
/// If Ollama is unreachable or returns no models, falls back to a
/// single entry for `default_model()` so session creation still
/// succeeds; the failure is logged to stderr.
fn build_models_state_from_listed(
    listed: Vec<String>,
    preferred_model: Option<&str>,
) -> (SessionModelState, String) {
    let default = ollama::default_model();

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

    let available_models = listed
        .into_iter()
        .map(|name| SessionModel {
            id: name.clone(),
            name,
            description: None,
            provided_by: ComponentSource {
                component_id: "local:ollama-provider".to_string(),
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

async fn build_models_state(preferred_model: Option<&str>) -> (SessionModelState, String) {
    let default = ollama::default_model();
    let listed = match ollama::list_models().await {
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
    build_models_state_from_listed(listed, preferred_model)
}

fn next_session_id() -> String {
    // Mix wall-clock seconds with a per-process counter so ids are unique
    // across host restarts. The editor's chat panel often retains the last
    // session id and replays it via `session/load` after a host restart;
    // returning the *same* id from a fresh `next_session_id()` call would
    // make distinct sessions collide on disk.
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("ollama-session-{secs:x}-{n}")
}

fn err(code: ErrorCode, message: &str) -> Error {
    Error {
        code,
        message: message.to_string(),
    }
}

/// Look up the absolute working directory associated with a session.
/// Returns the empty string when the session is unknown (which can happen
/// in tests or if `prompt` is reached without a prior `new-session`).
pub fn session_cwd(session_id: &str) -> String {
    SESSIONS.with(|s| {
        s.borrow()
            .get(session_id)
            .map(|sess| sess.cwd.clone())
            .unwrap_or_default()
    })
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
                name: "ollama-wasm-agent".to_string(),
                title: Some("Ollama (wasm)".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            auth_methods: Vec::new(),
        })
    }

    async fn authenticate(_req: AuthenticateRequest) -> Result<(), Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "authentication not required",
        ))
    }

    async fn new_session(req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let id = next_session_id();
        let (models, current_model) = build_models_state(None).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                Session {
                    history: Vec::new(),
                    model: current_model,
                    cwd: req.cwd,
                },
            )
        });
        Ok(NewSessionResponse {
            session_id: id,
            modes: None,
            models: Some(models),
        })
    }

    async fn load_session(req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
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
            client::update_session(req.session_id.clone(), update).await;
        }
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                req.session_id.clone(),
                Session {
                    history,
                    model: current_model,
                    cwd: req.cwd,
                },
            );
        });
        Ok(LoadSessionResponse {
            modes: None,
            models: Some(models),
        })
    }

    async fn list_sessions(_req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "list-sessions not supported",
        ))
    }

    async fn resume_session(req: ResumeSessionRequest) -> Result<ResumeSessionResponse, Error> {
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
        let (models, current_model) = build_models_state(preferred.as_deref()).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                req.session_id,
                Session {
                    history,
                    model: current_model,
                    cwd: req.cwd,
                },
            );
        });
        Ok(ResumeSessionResponse {
            modes: None,
            models: Some(models),
        })
    }

    async fn close_session(_session_id: SessionId) -> Result<(), Error> {
        Err(err(
            ErrorCode::MethodNotFound,
            "close-session not supported",
        ))
    }

    async fn set_session_mode(_req: SetSessionModeRequest) -> Result<(), Error> {
        // Provider advertises no modes; the slot is reserved for layers
        // (e.g. `plan-layer`) to manage. Anything reaching the provider
        // is therefore an unknown mode id.
        Err(err(
            ErrorCode::InvalidParams,
            "ollama provider does not advertise any modes",
        ))
    }

    async fn select_model(req: SelectModelRequest) -> Result<(), Error> {
        let SelectModelRequest {
            session_id,
            model_id,
        } = req;
        SESSIONS.with(|s| {
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

    async fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
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
        //
        // We also prepend a one-time `system` message on the first prompt
        // of a session, instructing the model to be conservative about
        // tool calls. Without this, small tool-capable models tend to call
        // `read_file` on greetings like "hello" out of pure enthusiasm.
        let (mut history, model) = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            let entry = sessions
                .entry(req.session_id.clone())
                .or_insert_with(|| Session {
                    history: Vec::new(),
                    model: ollama::default_model(),
                    cwd: String::new(),
                });
            if entry.history.is_empty() {
                let mut prompt = SYSTEM_PROMPT.to_string();
                if !entry.cwd.is_empty() {
                    prompt.push_str("\n\nThe user's current project directory is `");
                    prompt.push_str(&entry.cwd);
                    prompt.push_str(
                        "`. When the user refers to files without an absolute path, \
                         resolve them relative to this directory. Never call `read_file` \
                         with `/`, `.`, `..`, the project directory itself, or any other \
                         directory - only call it with a path to a specific file.",
                    );
                }
                entry.history.push(Message {
                    role: "system".to_string(),
                    content: prompt,
                    tool_calls: Vec::new(),
                });
            }
            entry.history.push(Message::user(user_text.clone()));
            (entry.history.clone(), entry.model.clone())
        });

        // Probe whether the active model supports tool-calling. We only
        // advertise tools when it does; otherwise the request is plain
        // chat. The probe is best-effort: if `/api/show` fails we just
        // assume no tool support and degrade to chat. We also surface a
        // one-time thought chunk so users understand why their model
        // isn't using tools.
        let session_id = req.session_id.clone();
        let model_clone = model.clone();
        let tools_supported = ollama::supports_tools(&model_clone).await.unwrap_or(false);
        if !tools_supported {
            client::update_session(
                session_id.clone(),
                SessionUpdate::AgentThoughtChunk(ContentBlock::Text(TextContent {
                    text: format!(
                        "(model `{model}` does not advertise tool support; running in chat-only mode)"
                    ),
                })),
            )
            .await;
        }
        let advertised_tools = if tools_supported {
            tools::ollama_tools()
        } else {
            Vec::new()
        };

        // Tool-call loop. The model can answer in one turn (no tool calls
        // → done) or it can request tools, in which case we dispatch each,
        // append a `role: "tool"` message per result, and loop. We cap
        // iterations to avoid runaway models.
        const MAX_TURNS: usize = 3;
        let mut tool_call_seq: u64 = 0;
        let mut stop = StopReason::EndTurn;
        let mut turns_remaining = MAX_TURNS;
        loop {
            if turns_remaining == 0 {
                stop = StopReason::MaxTurnRequests;
                break;
            }
            turns_remaining -= 1;

            let session_id_chunk = session_id.clone();
            let turn = ollama::chat(
                model.clone(),
                history.clone(),
                advertised_tools.clone(),
                |chunk| {
                    let sid = session_id_chunk.clone();
                    async move {
                        client::update_session(
                            sid,
                            SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent {
                                text: chunk,
                            })),
                        )
                        .await;
                    }
                },
            )
            .await
            .map_err(|e| err(ErrorCode::InternalError, &format!("ollama: {e}")))?;

            // Persist the assistant turn (text + any tool-call requests)
            // back into history so the next ollama call sees it.
            history.push(Message::assistant(
                turn.content.clone(),
                turn.tool_calls.clone(),
            ));

            if turn.tool_calls.is_empty() {
                break;
            }

            // Dispatch each tool call. For each one, send the editor a
            // `tool_call` notification (status: in_progress), run the
            // tool, send a `tool_call_update` (status: completed/failed
            // with content), and feed the result back as a `role: "tool"`
            // message for the next iteration.
            use acp_wasm_sys::provider::yosh::acp::content::ContentBlock as Cb;
            use acp_wasm_sys::provider::yosh::acp::content::TextContent as Tc;
            use acp_wasm_sys::provider::yosh::acp::tools::{
                ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
            };
            for call in &turn.tool_calls {
                tool_call_seq += 1;
                let tc_id = format!("tc-{tool_call_seq}");
                let tool = tools::lookup(&call.function.name);
                let title = tools::render_title(&call.function.name, &call.function.arguments);
                let raw_input = serde_json::to_string(&call.function.arguments).ok();

                client::update_session(
                    session_id.clone(),
                    SessionUpdate::ToolCall(ToolCall {
                        id: tc_id.clone(),
                        title: title.clone(),
                        kind: tool.map(|t| t.kind).unwrap_or(ToolKind::Other),
                        status: ToolCallStatus::InProgress,
                        content: Vec::new(),
                        locations: Vec::new(),
                        raw_input: raw_input.clone(),
                        raw_output: None,
                    }),
                )
                .await;

                let outcome =
                    tools::dispatch(&call.function.name, &session_id, &call.function.arguments)
                        .await;

                let locations = if outcome.locations.is_empty() {
                    None
                } else {
                    use acp_wasm_sys::provider::yosh::acp::tools::ToolCallLocation;
                    Some(
                        outcome
                            .locations
                            .iter()
                            .map(|p| ToolCallLocation {
                                path: p.clone(),
                                line: None,
                            })
                            .collect(),
                    )
                };
                client::update_session(
                    session_id.clone(),
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        id: tc_id.clone(),
                        title: None,
                        kind: None,
                        status: Some(if outcome.failed {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        }),
                        content: Some(vec![ToolCallContent::Content(Cb::Text(Tc {
                            text: outcome.content.clone(),
                        }))]),
                        locations,
                        raw_input: None,
                        raw_output: Some(outcome.content.clone()),
                    }),
                )
                .await;

                // Feed the result back to the model as a `role: "tool"`
                // message. For failures, prefix with a clear marker so
                // small models don't mistake the error text for data
                // (Ollama's chat API doesn't carry an `is_error` flag).
                let tool_msg = if outcome.failed {
                    format!("Error: {}", outcome.content)
                } else {
                    outcome.content
                };
                history.push(Message::tool(tool_msg));
            }
        }

        // Replace the session's history with our updated copy and persist.
        let snapshot = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            sessions.get_mut(&req.session_id).map(|entry| {
                entry.history = history;
                entry.clone()
            })
        });
        if let Some(session) = snapshot {
            if let Err(e) = storage::save(&req.session_id, &session) {
                // Persistence is best-effort: a failed save shouldn't fail
                // the prompt turn (the user already saw the reply). Log
                // via a thought chunk so it's visible.
                client::update_session(
                    req.session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(ContentBlock::Text(TextContent {
                        text: format!("(failed to persist session: {e})"),
                    })),
                )
                .await;
            }
        }

        Ok(PromptResponse { stop_reason: stop })
    }

    async fn cancel(_session_id: SessionId) {
        // TODO: real cancellation. The host serializes all wasm calls behind
        // a single mutex, so `cancel` cannot run while `prompt` is in
        // flight. Implementing proper cancellation requires moving the
        // streaming loop off the lock and using a shared cancellation flag.
    }
}

acp_wasm_sys::provider::export!(Agent with_types_in acp_wasm_sys::provider);
