//! Type translation between the wasmtime-generated WIT types
//! (`yoshuawuyts::acp` interfaces) and the `agent_client_protocol::schema` types.
//!
//! Only covers the variants the MVP exercises (text content, end-turn,
//! agent-message-chunk, etc.). Anything we can't translate yields an error
//! that surfaces back to the editor as a JSON-RPC error.

use std::path::PathBuf;

use agent_client_protocol::schema;
use agent_client_protocol::{Error as AcpError, ErrorCode as AcpErrorCode};
use tracing::debug;

use crate::yoshuawuyts::acp::content::{ContentBlock, TextContent};
use crate::yoshuawuyts::acp::errors::{Error, ErrorCode};
use crate::yoshuawuyts::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use crate::yoshuawuyts::acp::init::{
    AuthenticateRequest, ClientCapabilities, FsCapabilities, ImplementationInfo, InitializeRequest,
    InitializeResponse,
};
use crate::yoshuawuyts::acp::prompts::{PromptRequest, PromptResponse, SessionUpdate, StopReason};
use crate::yoshuawuyts::acp::sessions::{
    EnvVar, HttpHeader, LoadSessionRequest, LoadSessionResponse, McpServer, McpServerHttp,
    McpServerSse, McpServerStdio, NewSessionRequest, NewSessionResponse, SessionId, SessionMode,
    SessionModeId, SessionModeState, SetSessionModeRequest,
};
use crate::yoshuawuyts::acp::tools::{
    ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};

// -----------------------------------------------------------------------------
// JSON synthesis helper
// -----------------------------------------------------------------------------

/// Some `agent_client_protocol::schema` response types are `non_exhaustive`.
/// We construct them via JSON to avoid depending on private fields. If the
/// schema shape ever drifts, surface the failure as an ACP `internal-error`
/// rather than panicking.
fn synth<T: serde::de::DeserializeOwned>(
    context: &'static str,
    json: serde_json::Value,
) -> Result<T, AcpError> {
    serde_json::from_value(json).map_err(|e| {
        let mut out = AcpError::internal_error();
        out.message = format!("{context}: schema drift: {e}");
        out
    })
}

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

/// Build a WIT `Error` with `method-not-found` semantics. Used by the host's
/// `client` interface stubs.
pub fn method_not_found(message: &str) -> Error {
    Error {
        code: ErrorCode::MethodNotFound,
        message: message.to_string(),
    }
}

/// Convert a WIT-side ACP `Error` (returned by the wasm guest) into the
/// JSON-RPC `Error` shape the `agent_client_protocol` crate expects.
pub fn wit_error_to_acp(e: Error) -> AcpError {
    let mut out = match e.code {
        ErrorCode::ParseError => AcpError::parse_error(),
        ErrorCode::InvalidRequest => AcpError::invalid_request(),
        ErrorCode::MethodNotFound => AcpError::method_not_found(),
        ErrorCode::InvalidParams => AcpError::invalid_params(),
        ErrorCode::InternalError => AcpError::internal_error(),
        ErrorCode::AuthRequired => AcpError::auth_required(),
        ErrorCode::ResourceNotFound => AcpError::resource_not_found(None),
        ErrorCode::Other(n) => {
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

/// Wrap an anyhow error (e.g. from instantiation) as an internal JSON-RPC
/// error.
pub fn anyhow_to_acp(context: &str, e: anyhow::Error) -> AcpError {
    let mut out = AcpError::internal_error();
    out.message = format!("{context}: {e:#}");
    out
}

/// Build a WIT `Error` with `internal-error` code and the given message.
/// Used for host-side transport failures bubbling back to the wasm guest.
pub fn internal_error(message: &str) -> Error {
    Error {
        code: ErrorCode::InternalError,
        message: message.to_string(),
    }
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

pub fn init_request_schema_to_wit(req: schema::InitializeRequest) -> InitializeRequest {
    let cc = &req.client_capabilities;
    InitializeRequest {
        protocol_version: pv_to_u32(&req.protocol_version),
        client_capabilities: ClientCapabilities {
            fs: FsCapabilities {
                read_text_file: cc.fs.read_text_file,
                write_text_file: cc.fs.write_text_file,
            },
            terminal: cc.terminal,
        },
        client_info: req.client_info.map(|i| ImplementationInfo {
            name: i.name,
            title: i.title,
            version: i.version,
        }),
    }
}

pub fn init_response_wit_to_schema(resp: InitializeResponse) -> schema::InitializeResponse {
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

pub fn authenticate_request_schema_to_wit(req: schema::AuthenticateRequest) -> AuthenticateRequest {
    AuthenticateRequest {
        method_id: req.method_id.to_string(),
    }
}

// -----------------------------------------------------------------------------
// New session
// -----------------------------------------------------------------------------

pub fn new_session_request_schema_to_wit(req: schema::NewSessionRequest) -> NewSessionRequest {
    NewSessionRequest {
        cwd: path_to_string(&req.cwd),
        mcp_servers: req.mcp_servers.into_iter().map(mcp_server_to_wit).collect(),
    }
}

pub fn new_session_response_wit_to_schema(
    resp: NewSessionResponse,
    component_id: &str,
) -> Result<schema::NewSessionResponse, AcpError> {
    // schema::NewSessionResponse is `non_exhaustive`. Roundtrip via JSON to
    // construct it without depending on the (unstable) field set.
    let mut json = serde_json::json!({ "sessionId": resp.session_id });
    if let Some(modes) = resp.modes {
        json["modes"] = session_mode_state_to_json(modes, component_id);
    }
    synth("new-session response", json)
}

// -----------------------------------------------------------------------------
// Load session
// -----------------------------------------------------------------------------

pub fn load_session_request_schema_to_wit(req: schema::LoadSessionRequest) -> LoadSessionRequest {
    LoadSessionRequest {
        session_id: req.session_id.0.to_string(),
        cwd: path_to_string(&req.cwd),
        mcp_servers: req.mcp_servers.into_iter().map(mcp_server_to_wit).collect(),
    }
}

/// Convert a WIT `LoadSessionResponse` into the schema shape, propagating
/// any `modes` the agent advertised so the editor can render its picker
/// for resumed sessions.
pub fn load_session_response_wit_to_schema(
    resp: LoadSessionResponse,
    component_id: &str,
) -> Result<schema::LoadSessionResponse, AcpError> {
    let mut json = serde_json::json!({});
    if let Some(modes) = resp.modes {
        json["modes"] = session_mode_state_to_json(modes, component_id);
    }
    synth("load-session response", json)
}

// -----------------------------------------------------------------------------
// Session modes
// -----------------------------------------------------------------------------

pub fn set_session_mode_request_schema_to_wit(
    req: schema::SetSessionModeRequest,
) -> SetSessionModeRequest {
    SetSessionModeRequest {
        session_id: req.session_id.0.to_string(),
        mode_id: req.mode_id.0.to_string(),
    }
}

/// Empty `SetSessionModeResponse`. Constructed via JSON because the schema
/// type is `non_exhaustive`.
pub fn empty_set_session_mode_response() -> Result<schema::SetSessionModeResponse, AcpError> {
    synth("set-session-mode response", serde_json::json!({}))
}

fn session_mode_state_to_json(state: SessionModeState, component_id: &str) -> serde_json::Value {
    let SessionModeState {
        current_mode_id,
        available_modes,
    } = state;
    serde_json::json!({
        "currentModeId": current_mode_id,
        "availableModes": available_modes
            .into_iter()
            .map(|m| session_mode_to_json(m, component_id))
            .collect::<Vec<_>>(),
    })
}

/// Split a component id into a `(namespace, name)` pair using `:` as the
/// separator. If the id has no `:`, it's treated as a bare name in the
/// implicit `local` namespace, mirroring how `wasm.toml` would refer to an
/// unregistered local component.
fn split_component_id(component_id: &str) -> (&str, &str) {
    match component_id.split_once(':') {
        Some((ns, name)) if !ns.is_empty() && !name.is_empty() => (ns, name),
        _ => ("local", component_id),
    }
}

fn session_mode_to_json(mode: SessionMode, component_id: &str) -> serde_json::Value {
    let SessionMode {
        id,
        name,
        description,
    } = mode;
    // Prefix the display name with the component's `namespace:name` so the
    // editor's mode picker shows which provider each model belongs to —
    // useful when several components are loaded side-by-side. The mode
    // `id` is left untouched so `set-session-mode` round-trips cleanly.
    let (ns, comp_name) = split_component_id(component_id);
    let display = format!("{ns}:{comp_name} - {name}");
    let mut entry = serde_json::json!({ "id": id, "name": display });
    if let Some(d) = description {
        entry["description"] = serde_json::Value::String(d);
    }
    entry
}

/// Suppress the unused-import lint for `SessionModeId` — it's part of the
/// public WIT surface but not directly referenced in this module.
const _: fn(SessionModeId) = |_| {};

// -----------------------------------------------------------------------------
// Prompt
// -----------------------------------------------------------------------------

pub fn prompt_request_schema_to_wit(req: schema::PromptRequest) -> PromptRequest {
    PromptRequest {
        session_id: req.session_id.0.to_string(),
        prompt: req
            .prompt
            .into_iter()
            .filter_map(content_block_schema_to_wit)
            .collect(),
    }
}

pub fn prompt_response_wit_to_schema(
    resp: PromptResponse,
) -> Result<schema::PromptResponse, AcpError> {
    let stop_reason = match resp.stop_reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
    };
    synth(
        "prompt response",
        serde_json::json!({ "stopReason": stop_reason }),
    )
}

/// Build a `PromptResponse` with `stop_reason: cancelled`. The protocol
/// requires the agent to return this when a `session/cancel` notification
/// arrives during a prompt turn.
pub fn synthesised_cancelled_response() -> Result<schema::PromptResponse, AcpError> {
    prompt_response_wit_to_schema(PromptResponse {
        stop_reason: StopReason::Cancelled,
    })
}

/// Empty `AuthenticateResponse`. Constructed via JSON because the schema
/// type is `non_exhaustive`.
pub fn empty_authenticate_response() -> Result<schema::AuthenticateResponse, AcpError> {
    synth("authenticate response", serde_json::json!({}))
}

// -----------------------------------------------------------------------------
// Session updates (wasm → editor)
// -----------------------------------------------------------------------------

/// Convert a WIT-side `update-session` call into a schema
/// `SessionNotification` to forward to the editor. Returns `None` for variants
/// we don't yet support.
pub fn session_update_wit_to_schema(
    session_id: SessionId,
    update: SessionUpdate,
) -> Option<schema::SessionNotification> {
    let block = match update {
        SessionUpdate::AgentMessageChunk(b) => Some(("agent", b)),
        SessionUpdate::AgentThoughtChunk(b) => Some(("thought", b)),
        SessionUpdate::UserMessageChunk(b) => Some(("user", b)),
        SessionUpdate::ToolCall(call) => {
            let upd = tool_call_to_schema_update(&session_id, call)?;
            return Some(schema::SessionNotification::new(
                schema::SessionId::from(session_id),
                upd,
            ));
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let upd = tool_call_update_to_schema_update(&session_id, update)?;
            return Some(schema::SessionNotification::new(
                schema::SessionId::from(session_id),
                upd,
            ));
        }
        SessionUpdate::Plan(_) => {
            debug!(session = %session_id, "dropped session update: plan (not yet wired)");
            None
        }
        SessionUpdate::CurrentModeUpdate(mode_id) => {
            // The guest reports a mode switch (e.g. user picked a
            // different model). Forward as a real `current-mode-update`
            // notification so the editor's picker can reflect it.
            let upd: schema::SessionUpdate = serde_json::from_value(serde_json::json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": mode_id,
            }))
            .ok()?;
            return Some(schema::SessionNotification::new(
                schema::SessionId::from(session_id),
                upd,
            ));
        }
        SessionUpdate::SessionInfoUpdate(_) => {
            debug!(session = %session_id, "dropped session update: session-info-update (not yet wired)");
            None
        }
        SessionUpdate::AvailableCommandsUpdate(_) => {
            debug!(session = %session_id, "dropped session update: available-commands-update (not yet wired)");
            None
        }
    }?;
    let (kind, b) = block;
    let schema_block = content_block_wit_to_schema(&session_id, b)?;
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

fn content_block_schema_to_wit(block: schema::ContentBlock) -> Option<ContentBlock> {
    Some(match block {
        schema::ContentBlock::Text(t) => ContentBlock::Text(TextContent { text: t.text }),
        // Non-text variants ignored for MVP; the wasm guest only handles text.
        other => {
            debug!(
                variant = ?std::mem::discriminant(&other),
                "dropped inbound content block: non-text variant not yet supported"
            );
            return None;
        }
    })
}

fn content_block_wit_to_schema(
    session_id: &str,
    block: ContentBlock,
) -> Option<schema::ContentBlock> {
    Some(match block {
        ContentBlock::Text(t) => schema::ContentBlock::Text(schema::TextContent::new(t.text)),
        _ => {
            debug!(
                session = %session_id,
                "dropped outbound content block: non-text variant not yet supported"
            );
            return None;
        }
    })
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn path_to_string(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

fn mcp_server_to_wit(s: schema::McpServer) -> McpServer {
    match s {
        schema::McpServer::Stdio(server) => McpServer::Stdio(McpServerStdio {
            name: server.name,
            command: path_to_string(&PathBuf::from(server.command)),
            args: server.args,
            env: server
                .env
                .into_iter()
                .map(|e| EnvVar {
                    name: e.name,
                    value: e.value,
                })
                .collect(),
        }),
        schema::McpServer::Http(server) => McpServer::Http(McpServerHttp {
            name: server.name,
            url: server.url.to_string(),
            headers: server
                .headers
                .into_iter()
                .map(|h| HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
        }),
        schema::McpServer::Sse(server) => McpServer::Sse(McpServerSse {
            name: server.name,
            url: server.url.to_string(),
            headers: server
                .headers
                .into_iter()
                .map(|h| HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
        }),
        // Schema enum is `non_exhaustive`; future variants are dropped to a
        // stub stdio entry so we don't crash on protocol additions.
        other => {
            debug!(
                variant = ?std::mem::discriminant(&other),
                "unknown McpServer variant: substituting empty stdio stub"
            );
            McpServer::Stdio(McpServerStdio {
                name: String::from("<unknown>"),
                command: String::new(),
                args: Vec::new(),
                env: Vec::new(),
            })
        }
    }
}

// -----------------------------------------------------------------------------
// File system (wasm → editor)
// -----------------------------------------------------------------------------

pub fn read_text_file_request_wit_to_schema(
    req: ReadTextFileRequest,
) -> schema::ReadTextFileRequest {
    let mut out = schema::ReadTextFileRequest::new(
        schema::SessionId::from(req.session_id),
        PathBuf::from(req.path),
    );
    out.line = req.line;
    out.limit = req.limit;
    out
}

pub fn read_text_file_response_schema_to_wit(
    resp: schema::ReadTextFileResponse,
) -> ReadTextFileResponse {
    ReadTextFileResponse {
        content: resp.content,
    }
}

pub fn write_text_file_request_wit_to_schema(
    req: WriteTextFileRequest,
) -> schema::WriteTextFileRequest {
    schema::WriteTextFileRequest::new(
        schema::SessionId::from(req.session_id),
        PathBuf::from(req.path),
        req.content,
    )
}

/// Convert an ACP JSON-RPC error into a WIT error to return to the wasm
/// guest. Inverse of [`wit_error_to_acp`].
pub fn acp_error_to_wit(e: AcpError) -> Error {
    let code = match e.code {
        schema::ErrorCode::ParseError => ErrorCode::ParseError,
        schema::ErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
        schema::ErrorCode::MethodNotFound => ErrorCode::MethodNotFound,
        schema::ErrorCode::InvalidParams => ErrorCode::InvalidParams,
        schema::ErrorCode::InternalError => ErrorCode::InternalError,
        schema::ErrorCode::AuthRequired => ErrorCode::AuthRequired,
        schema::ErrorCode::ResourceNotFound => ErrorCode::ResourceNotFound,
        AcpErrorCode::Other(n) => ErrorCode::Other(n),
        // schema::ErrorCode is `non_exhaustive`. Anything new gets reported
        // as InternalError to keep our errors well-typed.
        _ => ErrorCode::InternalError,
    };
    Error {
        code,
        message: e.message,
    }
}

// -----------------------------------------------------------------------------
// Tool calls (wasm → editor)
// -----------------------------------------------------------------------------

/// Map our WIT `ToolKind` to the ACP wire string.
fn tool_kind_str(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::Other => "other",
    }
}

/// Map our WIT `ToolCallStatus` to the ACP wire string.
fn tool_status_str(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
    }
}

/// Render a `ToolCallContent` element into the JSON object the wire
/// expects. Currently only the `content` (standard content block) variant
/// is forwarded — diff and terminal variants are logged-and-dropped until
/// they're needed.
fn tool_call_content_to_json(
    session_id: &str,
    content: ToolCallContent,
) -> Option<serde_json::Value> {
    match content {
        ToolCallContent::Content(block) => {
            let schema_block = content_block_wit_to_schema(session_id, block)?;
            let inner = serde_json::to_value(schema_block).ok()?;
            Some(serde_json::json!({ "type": "content", "content": inner }))
        }
        ToolCallContent::Diff(_) => {
            debug!(session = %session_id, "dropped tool-call diff content (not yet wired)");
            None
        }
        ToolCallContent::Terminal(_) => {
            debug!(session = %session_id, "dropped tool-call terminal content (not yet wired)");
            None
        }
    }
}

fn tool_call_to_schema_update(session_id: &str, call: ToolCall) -> Option<schema::SessionUpdate> {
    let content: Vec<serde_json::Value> = call
        .content
        .into_iter()
        .filter_map(|c| tool_call_content_to_json(session_id, c))
        .collect();
    let raw_input = call
        .raw_input
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let raw_output = call
        .raw_output
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .or_else(|| call.raw_output.map(serde_json::Value::String));
    let mut json = serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call.id,
        "title": call.title,
        "kind": tool_kind_str(call.kind),
        "status": tool_status_str(call.status),
        "content": content,
    });
    if let Some(v) = raw_input {
        json["rawInput"] = v;
    }
    if let Some(v) = raw_output {
        json["rawOutput"] = v;
    }
    serde_json::from_value(json).ok()
}

fn tool_call_update_to_schema_update(
    session_id: &str,
    update: ToolCallUpdate,
) -> Option<schema::SessionUpdate> {
    let mut json = serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": update.id,
    });
    if let Some(t) = update.title {
        json["title"] = serde_json::Value::String(t);
    }
    if let Some(k) = update.kind {
        json["kind"] = serde_json::Value::String(tool_kind_str(k).to_string());
    }
    if let Some(s) = update.status {
        json["status"] = serde_json::Value::String(tool_status_str(s).to_string());
    }
    if let Some(content) = update.content {
        let arr: Vec<serde_json::Value> = content
            .into_iter()
            .filter_map(|c| tool_call_content_to_json(session_id, c))
            .collect();
        json["content"] = serde_json::Value::Array(arr);
    }
    if let Some(raw) = update.raw_input {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            json["rawInput"] = v;
        }
    }
    if let Some(raw) = update.raw_output {
        json["rawOutput"] = serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or(serde_json::Value::String(raw));
    }
    serde_json::from_value(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yoshuawuyts::acp::content::ImageContent;
    use crate::yoshuawuyts::acp::init::{
        AgentCapabilities, McpCapabilities, PromptCapabilities, SessionCapabilities,
    };

    #[test]
    fn error_code_roundtrip() {
        let cases = [
            (ErrorCode::ParseError, schema::ErrorCode::ParseError),
            (ErrorCode::InvalidRequest, schema::ErrorCode::InvalidRequest),
            (ErrorCode::MethodNotFound, schema::ErrorCode::MethodNotFound),
            (ErrorCode::InvalidParams, schema::ErrorCode::InvalidParams),
            (ErrorCode::InternalError, schema::ErrorCode::InternalError),
            (ErrorCode::AuthRequired, schema::ErrorCode::AuthRequired),
            (
                ErrorCode::ResourceNotFound,
                schema::ErrorCode::ResourceNotFound,
            ),
        ];
        for (wit_code, schema_code) in cases {
            let acp = wit_error_to_acp(Error {
                code: wit_code,
                message: "msg".into(),
            });
            assert_eq!(acp.code, schema_code, "code mismatch for {:?}", wit_code);
            assert_eq!(acp.message, "msg");
        }
    }

    #[test]
    fn error_code_other_passthrough() {
        let acp = wit_error_to_acp(Error {
            code: ErrorCode::Other(-12345),
            message: "boom".into(),
        });
        assert_eq!(acp.code, AcpErrorCode::Other(-12345));
        assert_eq!(acp.message, "boom");
    }

    #[test]
    fn protocol_version_roundtrip() {
        let n = pv_to_u32(&pv_from_u32(1));
        assert_eq!(n, 1);
    }

    #[test]
    fn initialize_request_translation() {
        let req = schema::InitializeRequest::new(schema::ProtocolVersion::V1)
            .client_info(schema::Implementation::new("editor", "1.0").title(Some("Ed".into())));
        let wit_req = init_request_schema_to_wit(req);
        assert_eq!(wit_req.protocol_version, 1);
        let info = wit_req.client_info.unwrap();
        assert_eq!(info.name, "editor");
        assert_eq!(info.version, "1.0");
        assert_eq!(info.title.as_deref(), Some("Ed"));
    }

    #[test]
    fn initialize_response_translation() {
        let resp = InitializeResponse {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: true,
                    audio: false,
                    embedded_context: false,
                },
                mcp_capabilities: McpCapabilities {
                    http: true,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: false,
                    resume: false,
                    close: false,
                },
            },
            agent_info: Some(ImplementationInfo {
                name: "ag".into(),
                title: None,
                version: "0.1".into(),
            }),
            auth_methods: vec![],
        };
        let schema_resp = init_response_wit_to_schema(resp);
        assert_eq!(pv_to_u32(&schema_resp.protocol_version), 1);
        assert!(schema_resp.agent_capabilities.load_session);
        assert!(schema_resp.agent_capabilities.prompt_capabilities.image);
        assert!(schema_resp.agent_capabilities.mcp_capabilities.http);
        let info = schema_resp.agent_info.unwrap();
        assert_eq!(info.name, "ag");
        assert_eq!(info.version, "0.1");
    }

    #[test]
    fn prompt_request_text_only() {
        let req = schema::PromptRequest::new(
            schema::SessionId::from("sess-1"),
            vec![schema::ContentBlock::Text(schema::TextContent::new(
                "hello",
            ))],
        );
        let wit_req = prompt_request_schema_to_wit(req);
        assert_eq!(wit_req.session_id, "sess-1");
        assert_eq!(wit_req.prompt.len(), 1);
        match &wit_req.prompt[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn prompt_response_stop_reasons() {
        for (wit_sr, json_sr) in [
            (StopReason::EndTurn, "end_turn"),
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::Refusal, "refusal"),
            (StopReason::Cancelled, "cancelled"),
        ] {
            let resp = prompt_response_wit_to_schema(PromptResponse {
                stop_reason: wit_sr,
            })
            .expect("synth ok");
            let json = serde_json::to_value(&resp).unwrap();
            assert_eq!(json["stopReason"], json_sr);
        }
    }

    #[test]
    fn session_update_agent_chunk() {
        let notif = session_update_wit_to_schema(
            "s1".into(),
            SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent { text: "hi".into() })),
        )
        .unwrap();
        let json = serde_json::to_value(&notif).unwrap();
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json["update"]["content"]["text"], "hi");
    }

    #[test]
    fn session_update_unsupported_drops() {
        // Non-text content blocks aren't translated yet, so the update is
        // dropped rather than panicking or sending malformed data.
        let dropped = session_update_wit_to_schema(
            "s".into(),
            SessionUpdate::AgentMessageChunk(ContentBlock::Image(ImageContent {
                data: "x".into(),
                mime_type: "image/png".into(),
                uri: None,
            })),
        );
        assert!(dropped.is_none());
    }
}
