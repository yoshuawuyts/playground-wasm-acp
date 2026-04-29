//! ACP wasm guest agent that forwards prompts to a local Ollama server.

mod ollama;

use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::exports::yoshuawuyts::acp::agent::Guest;
use acp_wasm_sys::yoshuawuyts::acp::client;
use acp_wasm_sys::yoshuawuyts::acp::types::{
    AgentCapabilities, AuthenticateRequest, ContentBlock, Error, ErrorCode, ImplementationInfo,
    InitializeRequest, InitializeResponse, LoadSessionRequest, McpCapabilities, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionId,
    SessionUpdate, StopReason, TextContent,
};

struct Agent;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        Ok(NewSessionResponse {
            session_id: next_session_id(),
        })
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

        let reply = wstd::runtime::block_on(ollama::chat(&user_text))
            .map_err(|e| err(ErrorCode::InternalError, &format!("ollama: {e}")))?;

        client::update_session(
            &req.session_id,
            &SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent { text: reply })),
        );

        Ok(PromptResponse {
            stop_reason: StopReason::EndTurn,
        })
    }

    fn cancel(_session_id: SessionId) {
        // No concurrency: nothing to cancel.
    }
}

acp_wasm_sys::export!(Agent with_types_in acp_wasm_sys);
