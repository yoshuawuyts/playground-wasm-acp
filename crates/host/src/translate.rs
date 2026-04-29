//! Type translation between the wasmtime-generated WIT types
//! (`yoshuawuyts::acp::types`) and the `agent_client_protocol::schema` types.
//!
//! Only covers the variants the MVP exercises (text content, end-turn,
//! agent-message-chunk, etc.). Anything we can't translate yields an error
//! that surfaces back to the editor as a JSON-RPC error.

use std::path::PathBuf;

use agent_client_protocol::schema;
use agent_client_protocol::{Error as AcpError, ErrorCode as AcpErrorCode};

use crate::yoshuawuyts::acp::types as wit;

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

/// Build a WIT `Error` with `method-not-found` semantics. Used by the host's
/// `client` interface stubs.
pub fn method_not_found(message: &str) -> wit::Error {
    wit::Error {
        code: wit::ErrorCode::MethodNotFound,
        message: message.to_string(),
    }
}

/// Convert a WIT-side ACP `Error` (returned by the wasm guest) into the
/// JSON-RPC `Error` shape the `agent_client_protocol` crate expects.
pub fn wit_error_to_acp(e: wit::Error) -> AcpError {
    let mut out = match e.code {
        wit::ErrorCode::ParseError => AcpError::parse_error(),
        wit::ErrorCode::InvalidRequest => AcpError::invalid_request(),
        wit::ErrorCode::MethodNotFound => AcpError::method_not_found(),
        wit::ErrorCode::InvalidParams => AcpError::invalid_params(),
        wit::ErrorCode::InternalError => AcpError::internal_error(),
        wit::ErrorCode::AuthRequired => AcpError::auth_required(),
        wit::ErrorCode::ResourceNotFound => AcpError::resource_not_found(None),
        wit::ErrorCode::Other(n) => {
            let mut e = AcpError::internal_error();
            e.code = AcpErrorCode::Other(n);
            e
        }
    };
    out.message = e.message;
    out
}

/// Wrap a wasmtime trap as an internal JSON-RPC error.
pub fn trap_to_acp(context: &str, e: wasmtime::Error) -> AcpError {
    let mut out = AcpError::internal_error();
    out.message = format!("{context}: {e:#}");
    out
}

// -----------------------------------------------------------------------------
// Protocol version
// -----------------------------------------------------------------------------

pub fn pv_to_u32(pv: &schema::ProtocolVersion) -> u32 {
    pv.to_string().parse::<u32>().unwrap_or(0)
}

pub fn pv_from_u32(n: u32) -> schema::ProtocolVersion {
    schema::ProtocolVersion::from(n as u16)
}

// -----------------------------------------------------------------------------
// Initialize
// -----------------------------------------------------------------------------

pub fn init_request_schema_to_wit(req: schema::InitializeRequest) -> wit::InitializeRequest {
    let cc = &req.client_capabilities;
    wit::InitializeRequest {
        protocol_version: pv_to_u32(&req.protocol_version),
        client_capabilities: wit::ClientCapabilities {
            fs: wit::FsCapabilities {
                read_text_file: cc.fs.read_text_file,
                write_text_file: cc.fs.write_text_file,
            },
            terminal: cc.terminal,
        },
        client_info: req.client_info.map(|i| wit::ImplementationInfo {
            name: i.name,
            title: i.title,
            version: i.version,
        }),
    }
}

pub fn init_response_wit_to_schema(resp: wit::InitializeResponse) -> schema::InitializeResponse {
    let agent_caps = schema::AgentCapabilities::new()
        .load_session(resp.agent_capabilities.load_session)
        .prompt_capabilities(
            schema::PromptCapabilities::new()
                .image(resp.agent_capabilities.prompt_capabilities.image)
                .audio(resp.agent_capabilities.prompt_capabilities.audio)
                .embedded_context(resp.agent_capabilities.prompt_capabilities.embedded_context),
        )
        .mcp_capabilities(
            schema::McpCapabilities::new()
                .http(resp.agent_capabilities.mcp_capabilities.http)
                .sse(resp.agent_capabilities.mcp_capabilities.sse),
        );

    let mut out = schema::InitializeResponse::new(pv_from_u32(resp.protocol_version))
        .agent_capabilities(agent_caps);
    if let Some(info) = resp.agent_info {
        let impl_ = schema::Implementation::new(info.name, info.version).title(info.title);
        out = out.agent_info(impl_);
    }
    out
}

// -----------------------------------------------------------------------------
// Authenticate
// -----------------------------------------------------------------------------

pub fn authenticate_request_schema_to_wit(
    req: schema::AuthenticateRequest,
) -> wit::AuthenticateRequest {
    wit::AuthenticateRequest {
        method_id: req.method_id.to_string(),
    }
}

// -----------------------------------------------------------------------------
// New session
// -----------------------------------------------------------------------------

pub fn new_session_request_schema_to_wit(
    req: schema::NewSessionRequest,
) -> wit::NewSessionRequest {
    wit::NewSessionRequest {
        cwd: path_to_string(&req.cwd),
        mcp_servers: req.mcp_servers.into_iter().map(mcp_server_to_wit).collect(),
    }
}

pub fn new_session_response_wit_to_schema(
    resp: wit::NewSessionResponse,
) -> schema::NewSessionResponse {
    // schema::NewSessionResponse is `non_exhaustive`. Roundtrip via JSON to
    // construct it without depending on the (unstable) field set.
    let json = serde_json::json!({ "sessionId": resp.session_id });
    serde_json::from_value(json).expect("NewSessionResponse JSON shape stable")
}

// -----------------------------------------------------------------------------
// Load session
// -----------------------------------------------------------------------------

pub fn load_session_request_schema_to_wit(
    req: schema::LoadSessionRequest,
) -> wit::LoadSessionRequest {
    wit::LoadSessionRequest {
        session_id: req.session_id.0.to_string(),
        cwd: path_to_string(&req.cwd),
        mcp_servers: req.mcp_servers.into_iter().map(mcp_server_to_wit).collect(),
    }
}

// -----------------------------------------------------------------------------
// Prompt
// -----------------------------------------------------------------------------

pub fn prompt_request_schema_to_wit(req: schema::PromptRequest) -> wit::PromptRequest {
    wit::PromptRequest {
        session_id: req.session_id.0.to_string(),
        prompt: req
            .prompt
            .into_iter()
            .filter_map(content_block_schema_to_wit)
            .collect(),
    }
}

pub fn prompt_response_wit_to_schema(resp: wit::PromptResponse) -> schema::PromptResponse {
    let stop_reason = match resp.stop_reason {
        wit::StopReason::EndTurn => "end_turn",
        wit::StopReason::MaxTokens => "max_tokens",
        wit::StopReason::MaxTurnRequests => "max_turn_requests",
        wit::StopReason::Refusal => "refusal",
        wit::StopReason::Cancelled => "cancelled",
    };
    let json = serde_json::json!({ "stopReason": stop_reason });
    serde_json::from_value(json).expect("PromptResponse JSON shape stable")
}

// -----------------------------------------------------------------------------
// Cancel
// -----------------------------------------------------------------------------

pub fn cancel_session_id_schema_to_wit(notif: &schema::CancelNotification) -> wit::SessionId {
    notif.session_id.0.to_string()
}

// -----------------------------------------------------------------------------
// Session updates (wasm → editor)
// -----------------------------------------------------------------------------

/// Convert a WIT-side `update-session` call into a schema
/// `SessionNotification` to forward to the editor. Returns `None` for variants
/// we don't yet support.
pub fn session_update_wit_to_schema(
    session_id: wit::SessionId,
    update: wit::SessionUpdate,
) -> Option<schema::SessionNotification> {
    let block = match update {
        wit::SessionUpdate::AgentMessageChunk(b) => Some(("agent", b)),
        wit::SessionUpdate::AgentThoughtChunk(b) => Some(("thought", b)),
        wit::SessionUpdate::UserMessageChunk(b) => Some(("user", b)),
        wit::SessionUpdate::ToolCall(_)
        | wit::SessionUpdate::ToolCallUpdate(_)
        | wit::SessionUpdate::Plan(_) => None,
    }?;
    let (kind, b) = block;
    let schema_block = content_block_wit_to_schema(b)?;
    let chunk = schema::ContentChunk::new(schema_block);
    let upd = match kind {
        "agent" => schema::SessionUpdate::AgentMessageChunk(chunk),
        "thought" => schema::SessionUpdate::AgentThoughtChunk(chunk),
        "user" => schema::SessionUpdate::UserMessageChunk(chunk),
        _ => unreachable!(),
    };
    Some(schema::SessionNotification::new(
        schema::SessionId::from(session_id),
        upd,
    ))
}

// -----------------------------------------------------------------------------
// Content blocks (text-only for MVP; non-text variants are dropped)
// -----------------------------------------------------------------------------

fn content_block_schema_to_wit(block: schema::ContentBlock) -> Option<wit::ContentBlock> {
    Some(match block {
        schema::ContentBlock::Text(t) => wit::ContentBlock::Text(wit::TextContent { text: t.text }),
        // Non-text variants ignored for MVP; the wasm guest only handles text.
        _ => return None,
    })
}

fn content_block_wit_to_schema(block: wit::ContentBlock) -> Option<schema::ContentBlock> {
    Some(match block {
        wit::ContentBlock::Text(t) => {
            schema::ContentBlock::Text(schema::TextContent::new(t.text))
        }
        _ => return None,
    })
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn path_to_string(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

fn mcp_server_to_wit(s: schema::McpServer) -> wit::McpServer {
    match s {
        schema::McpServer::Stdio(server) => wit::McpServer::Stdio(wit::McpServerStdio {
            name: server.name,
            command: path_to_string(&PathBuf::from(server.command)),
            args: server.args,
            env: server
                .env
                .into_iter()
                .map(|e| wit::EnvVar {
                    name: e.name,
                    value: e.value,
                })
                .collect(),
        }),
        schema::McpServer::Http(server) => wit::McpServer::Http(wit::McpServerHttp {
            name: server.name,
            url: server.url.to_string(),
            headers: server
                .headers
                .into_iter()
                .map(|h| wit::HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
        }),
        schema::McpServer::Sse(server) => wit::McpServer::Sse(wit::McpServerSse {
            name: server.name,
            url: server.url.to_string(),
            headers: server
                .headers
                .into_iter()
                .map(|h| wit::HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
        }),
        // Schema enum is `non_exhaustive`; future variants are dropped to a
        // stub stdio entry so we don't crash on protocol additions.
        _ => wit::McpServer::Stdio(wit::McpServerStdio {
            name: String::from("<unknown>"),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
        }),
    }
}
