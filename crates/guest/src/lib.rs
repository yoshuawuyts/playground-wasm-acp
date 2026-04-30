//! ACP wasm guest agent that forwards prompts to a local Ollama server.

mod ollama;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::exports::yoshuawuyts::acp::agent::Guest;
use acp_wasm_sys::yoshuawuyts::acp::client;
use acp_wasm_sys::yoshuawuyts::acp::types::{
    AgentCapabilities, AuthenticateRequest, ContentBlock, Error, ErrorCode, ImplementationInfo,
    InitializeRequest, InitializeResponse, LoadSessionRequest, McpCapabilities, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionId,
    SessionUpdate, StopReason, TextContent,
};

use crate::ollama::Message;

struct Agent;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

// Wasm components are single-threaded; using thread-local + RefCell avoids
// any synchronization cost while keeping interior mutability.
thread_local! {
    static SESSIONS: RefCell<HashMap<String, Vec<Message>>> = RefCell::new(HashMap::new());
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
                load_session: false,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                },
                mcp_capabilities: McpCapabilities {
                    http: false,
                    sse: false,
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
        SESSIONS.with(|s| s.borrow_mut().insert(id.clone(), Vec::new()));
        Ok(NewSessionResponse { session_id: id })
    }

    fn load_session(_req: LoadSessionRequest) -> Result<(), Error> {
        Err(err(ErrorCode::MethodNotFound, "load-session not supported"))
    }

    fn prompt(req: PromptRequest) -> Result<PromptResponse, Error> {
        let user_text = extract_user_text(&req.prompt);
        if user_text.is_empty() {
            return Err(err(
                ErrorCode::InvalidParams,
                "prompt contained no text content",
            ));
        }

        // Pull (or initialize) this session's running message history and
        // append the new user turn before sending.
        let history: Vec<Message> = SESSIONS.with(|s| {
            let mut sessions = s.borrow_mut();
            let entry = sessions.entry(req.session_id.clone()).or_default();
            entry.push(Message {
                role: "user".to_string(),
                content: user_text.clone(),
            });
            entry.clone()
        });

        let session_id = req.session_id.clone();
        let assistant = wstd::runtime::block_on(ollama::chat(&history, |chunk| {
            client::update_session(
                &session_id,
                &SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent {
                    text: chunk.to_string(),
                })),
            );
        }))
        .map_err(|e| err(ErrorCode::InternalError, &format!("ollama: {e}")))?;

        // Record the assistant's reply for the next turn.
        SESSIONS.with(|s| {
            if let Some(entry) = s.borrow_mut().get_mut(&req.session_id) {
                entry.push(Message {
                    role: "assistant".to_string(),
                    content: assistant,
                });
            }
        });

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
