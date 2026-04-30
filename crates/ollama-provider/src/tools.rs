//! Tools the language model can call during a prompt turn.
//!
//! Each tool has:
//!   - a name (must match what we advertise to Ollama),
//!   - a `ToolKind` for the editor's UI hint,
//!   - a description (the model uses this to decide when to call it),
//!   - a JSON-schema for its arguments,
//!   - a runner that dispatches the call into the host's `client`
//!     interface (or, in future, into peer sessions).
//!
//! For now there's a single tool, `read_file`, which forwards to ACP's
//! `fs/read_text_file`. Adding more tools is "add another `Tool` to
//! [`all`]" — no boilerplate beyond the schema.

use acp_wasm_sys::yoshuawuyts::acp::client;
use acp_wasm_sys::yoshuawuyts::acp::filesystem::ReadTextFileRequest;
use acp_wasm_sys::yoshuawuyts::acp::tools::ToolKind;
use serde_json::{Value, json};

use crate::ollama::{OllamaTool, OllamaToolCall};

/// Outcome of running a tool. Either a string result to feed back into
/// the model, or an error message that surfaces to the user as a failed
/// tool-call status.
pub struct ToolOutcome {
    pub content: String,
    pub failed: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failed: false,
        }
    }

    pub fn fail(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failed: true,
        }
    }
}

/// One tool the model can call.
pub struct Tool {
    pub name: &'static str,
    pub kind: ToolKind,
    pub description: &'static str,
    /// JSON-schema for the function arguments.
    pub parameters: fn() -> Value,
    /// Runner. Receives the session id (for any nested ACP calls that
    /// need it) and the model-supplied arguments. Failure is reported
    /// via [`ToolOutcome::fail`], not via `Result::Err` — the tool-call
    /// loop wants to feed *something* back to the model in either case.
    pub run: fn(session_id: &str, args: &Value) -> ToolOutcome,
}

impl Tool {
    /// Build the per-call `ToolCall` payload (id, title, status pending)
    /// for the editor. The id namespaces tool-call updates within a
    /// session; we use a simple incrementing counter passed in.
    pub fn ollama_tool(&self) -> OllamaTool {
        OllamaTool::function(self.name, self.description, (self.parameters)())
    }
}

/// All tools enabled for this provider. Order is informational; the model
/// addresses tools by name.
pub fn all() -> &'static [Tool] {
    static TOOLS: &[Tool] = &[Tool {
        name: "read_file",
        kind: ToolKind::Read,
        description: "Read a UTF-8 text file from the user's project. \
                      Use this to inspect source files the user mentions \
                      or that you need to understand. Path may be \
                      absolute, or relative to the project root.",
        parameters: read_file_schema,
        run: read_file_run,
    }];
    TOOLS
}

/// Look up a tool by the name the model used in its `tool_calls` entry.
pub fn lookup(name: &str) -> Option<&'static Tool> {
    all().iter().find(|t| t.name == name)
}

fn read_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the file. Absolute, or relative to the project root."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

fn read_file_run(session_id: &str, args: &Value) -> ToolOutcome {
    // Models inconsistently send arguments as a JSON object or as a
    // JSON-encoded string. Accept both.
    let args = match args {
        Value::Object(_) => args.clone(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::fail(format!("invalid arguments JSON: {e}")),
        },
        _ => return ToolOutcome::fail("expected an object of arguments".to_string()),
    };
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return ToolOutcome::fail("missing required argument `path`".to_string());
    };
    let req = ReadTextFileRequest {
        session_id: session_id.to_string(),
        path: path.to_string(),
        line: None,
        limit: None,
    };
    match client::read_text_file(&req) {
        Ok(resp) => ToolOutcome::ok(resp.content),
        Err(e) => ToolOutcome::fail(format!("read_text_file({path}): {}", e.message)),
    }
}

/// Render the tool-call args (whatever the model gave us) into a short
/// human-readable title for the ACP `ToolCall.title` field.
pub fn render_title(name: &str, args: &Value) -> String {
    match name {
        "read_file" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| args.as_str())
                .unwrap_or("?");
            format!("Reading {path}")
        }
        _ => name.to_string(),
    }
}

/// Convenience: build the `tools[]` array we send to Ollama.
pub fn ollama_tools() -> Vec<OllamaTool> {
    all().iter().map(|t| t.ollama_tool()).collect()
}

/// Marker so `OllamaToolCall` is referenced in this module — keeps the
/// `use` import alive for future tool dispatch helpers.
#[allow(dead_code)]
fn _unused_marker(_: &OllamaToolCall) {}
