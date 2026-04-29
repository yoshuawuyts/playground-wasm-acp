//! Type translation between the wasmtime-generated WIT types
//! (`yoshuawuyts::acp::types`) and the `agent_client_protocol::schema` types,
//! plus small helpers used by the host.
//!
//! The MVP only needs a tiny subset; this module is deliberately minimal and
//! grows as we wire more methods through.

use crate::yoshuawuyts::acp::types as wit;

/// Build a WIT `Error` with `method-not-found` semantics.
pub fn method_not_found(message: &str) -> wit::Error {
    wit::Error {
        code: wit::ErrorCode::MethodNotFound,
        message: message.to_string(),
    }
}

/// Best-effort one-line debug summary for a `SessionUpdate`. Used for stderr
/// logging until we wire updates through to the editor.
pub fn session_update_summary(update: &wit::SessionUpdate) -> String {
    match update {
        wit::SessionUpdate::UserMessageChunk(c) => format!("user-message-chunk({})", content_summary(c)),
        wit::SessionUpdate::AgentMessageChunk(c) => {
            format!("agent-message-chunk({})", content_summary(c))
        }
        wit::SessionUpdate::AgentThoughtChunk(c) => {
            format!("agent-thought-chunk({})", content_summary(c))
        }
        wit::SessionUpdate::ToolCall(_) => "tool-call".to_string(),
        wit::SessionUpdate::ToolCallUpdate(_) => "tool-call-update".to_string(),
        wit::SessionUpdate::Plan(_) => "plan".to_string(),
    }
}

fn content_summary(block: &wit::ContentBlock) -> String {
    match block {
        wit::ContentBlock::Text(t) => format!("text {:?}", truncate(&t.text, 60)),
        wit::ContentBlock::Image(_) => "image".to_string(),
        wit::ContentBlock::Audio(_) => "audio".to_string(),
        wit::ContentBlock::ResourceLink(_) => "resource-link".to_string(),
        wit::ContentBlock::Resource(_) => "resource".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
