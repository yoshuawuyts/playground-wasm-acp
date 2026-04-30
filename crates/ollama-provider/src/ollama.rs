//! Minimal HTTP client for the Ollama `/api/chat` endpoint, with streaming
//! support and (optional) tool-calling.
//!
//! Endpoint and model are configurable via `OLLAMA_URL` and `OLLAMA_MODEL`
//! environment variables; see [`endpoint()`] and [`default_model()`].

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wstd::http::{Body, BodyExt, Client, Method, Request};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/api/chat";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";

/// The configured Ollama `/api/chat` endpoint. Uses `OLLAMA_URL` if set.
pub fn endpoint() -> String {
    std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string())
}

/// The configured default Ollama model name. Uses `OLLAMA_MODEL` if set.
pub fn default_model() -> String {
    std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string())
}

/// The Ollama base URL (no path). Derived from [`endpoint()`] by stripping
/// a trailing `/api/...` segment, so `OLLAMA_URL` remains the only knob.
fn base_url() -> String {
    let ep = endpoint();
    if let Some(idx) = ep.find("/api/") {
        ep[..idx].to_string()
    } else {
        ep
    }
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
}

/// List models installed in the local Ollama via `GET {base}/api/tags`.
/// Returns names in the order Ollama reports them.
pub async fn list_models() -> Result<Vec<String>, String> {
    let url = format!("{}/api/tags", base_url());
    let req = Request::builder()
        .method(Method::GET)
        .uri(&url)
        .body(Body::empty())
        .map_err(|e| format!("build request: {e}"))?;

    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .body_mut()
            .str_contents()
            .await
            .unwrap_or("<unreadable>")
            .to_string();
        return Err(format!("ollama HTTP {status}: {txt}"));
    }
    let body = resp
        .body_mut()
        .json::<TagsResponse>()
        .await
        .map_err(|e| format!("decode tags: {e}"))?;
    Ok(body.models.into_iter().map(|m| m.name).collect())
}

/// One message in an Ollama chat history. Owned form so we can keep history
/// across prompt turns.
///
/// `tool_calls` is populated on assistant messages that requested tools;
/// the server expects to see those echoed back in the next request so it
/// can correlate the subsequent tool-result messages.
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OllamaToolCall>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<OllamaToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_calls,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }
}

/// A tool definition advertised to the model on every `/api/chat` request.
/// Mirrors Ollama's `tools[]` element shape.
#[derive(Clone, Serialize)]
pub struct OllamaTool {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function" today
    pub function: OllamaFunction,
}

#[derive(Clone, Serialize)]
pub struct OllamaFunction {
    pub name: String,
    pub description: String,
    /// JSON schema for the function arguments (a JSON object schema).
    pub parameters: Value,
}

impl OllamaTool {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function",
            function: OllamaFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// A tool call requested by the model. We round-trip these back into
/// history (on assistant messages) so Ollama can correlate the followup
/// tool-result message we send.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct OllamaToolCall {
    pub function: OllamaToolCallFunction,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct OllamaToolCallFunction {
    pub name: String,
    /// Free-form JSON object of arguments. Some models stream this as a
    /// JSON object directly, others as a JSON-encoded string.
    pub arguments: Value,
}

#[derive(Serialize)]
struct ChatRequestOwned<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "<[OllamaTool]>::is_empty")]
    tools: &'a [OllamaTool],
}

/// One streamed chunk from `/api/chat` with `stream: true`. Ollama sends a
/// JSON object per line; the final one has `done: true` and an empty
/// `message.content`. Tool calls (when supported) arrive on the same
/// `message` field as `tool_calls`.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    message: Option<StreamMessage>,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct StreamMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

/// Result of one `/api/chat` turn: the assembled assistant text plus any
/// tool calls the model requested. If `tool_calls` is non-empty, the
/// caller should dispatch each, append the results as `role: "tool"`
/// messages, and call [`chat`] again so the model can incorporate them.
pub struct ChatTurn {
    pub content: String,
    pub tool_calls: Vec<OllamaToolCall>,
}

/// Send a streaming chat completion to Ollama. `on_chunk` is invoked once
/// per non-empty content fragment as it arrives.
///
/// `tools` is the (possibly empty) list advertised to the model; pass
/// `&[]` for plain chat. Models that don't support tools simply ignore
/// the field — but you can probe with [`supports_tools`] to skip sending
/// the array entirely.
pub async fn chat<F: FnMut(&str)>(
    model: &str,
    history: &[Message],
    tools: &[OllamaTool],
    on_chunk: F,
) -> Result<ChatTurn, String> {
    let url = endpoint();
    let body = Body::from_json(&ChatRequestOwned {
        model,
        messages: history,
        stream: true,
        tools,
    })
    .map_err(|e| format!("encode: {e}"))?;

    let req = Request::builder()
        .method(Method::POST)
        .uri(&url)
        .header("content-type", "application/json")
        .body(body)
        .map_err(|e| format!("build request: {e}"))?;

    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .body_mut()
            .str_contents()
            .await
            .unwrap_or("<unreadable>")
            .to_string();
        return Err(format!("ollama HTTP {status}: {txt}"));
    }

    let mut stream = resp.into_body().into_boxed_body().into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut tool_calls: Vec<OllamaToolCall> = Vec::new();
    let mut on_chunk = on_chunk;
    let mut absorb = |chunk: StreamChunk| -> bool {
        if let Some(msg) = chunk.message {
            if !msg.content.is_empty() {
                on_chunk(&msg.content);
                content.push_str(&msg.content);
            }
            if !msg.tool_calls.is_empty() {
                tool_calls.extend(msg.tool_calls);
            }
        }
        chunk.done
    };
    let mut done = false;
    'outer: while let Some(frame) = stream.next().await {
        let bytes = frame.map_err(|e| format!("read body: {e}"))?;
        buf.extend_from_slice(&bytes);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let chunk: StreamChunk =
                serde_json::from_slice(line).map_err(|e| format!("decode chunk: {e}"))?;
            if absorb(chunk) {
                done = true;
                break 'outer;
            }
        }
    }
    if !done && !buf.is_empty() {
        if let Ok(chunk) = serde_json::from_slice::<StreamChunk>(&buf) {
            absorb(chunk);
        }
    }
    Ok(ChatTurn { content, tool_calls })
}

// -----------------------------------------------------------------------------
// Capability check (`/api/show`)
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct ShowRequest<'a> {
    model: &'a str,
}

#[derive(Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Probe Ollama for whether the given model declares tool-calling support
/// (i.e. its `/api/show` response includes `"tools"` in `capabilities`).
///
/// Network/HTTP failures bubble up; an unrecognised model returns
/// `Ok(false)` rather than an error so the caller can degrade gracefully
/// to plain chat instead of failing the prompt turn.
pub async fn supports_tools(model: &str) -> Result<bool, String> {
    let url = format!("{}/api/show", base_url());
    let body = Body::from_json(&ShowRequest { model }).map_err(|e| format!("encode: {e}"))?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(&url)
        .header("content-type", "application/json")
        .body(body)
        .map_err(|e| format!("build request: {e}"))?;
    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Ok(false);
    }
    let body = resp
        .body_mut()
        .json::<ShowResponse>()
        .await
        .map_err(|e| format!("decode show: {e}"))?;
    Ok(body.capabilities.iter().any(|c| c == "tools"))
}
