//! Minimal HTTP client for the Ollama `/api/chat` endpoint, with streaming
//! support.
//!
//! Endpoint and model are configurable via `OLLAMA_URL` and `OLLAMA_MODEL`
//! environment variables; see [`endpoint()`] and [`default_model()`].

use futures::StreamExt;
use serde::{Deserialize, Serialize};
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

/// One message in an Ollama chat history. Owned form so we can keep history
/// across prompt turns.
#[derive(Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Serialize)]
struct ChatRequestOwned<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
}

/// One streamed chunk from `/api/chat` with `stream: true`. Ollama sends a
/// JSON object per line; the final one has `done: true` and an empty
/// `message.content`.
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
}

/// Send a streaming chat completion to Ollama. `on_chunk` is invoked once per
/// non-empty content fragment as it arrives. Returns the assembled assistant
/// reply text once the server marks the stream as done.
pub async fn chat<F: FnMut(&str)>(history: &[Message], on_chunk: F) -> Result<String, String> {
    let model = default_model();
    let url = endpoint();
    let body = Body::from_json(&ChatRequestOwned {
        model: &model,
        messages: history,
        stream: true,
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
    let mut assistant = String::new();
    let mut on_chunk = on_chunk;
    while let Some(frame) = stream.next().await {
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
            if let Some(msg) = chunk.message {
                if !msg.content.is_empty() {
                    on_chunk(&msg.content);
                    assistant.push_str(&msg.content);
                }
            }
            if chunk.done {
                return Ok(assistant);
            }
        }
    }
    if !buf.is_empty() {
        if let Ok(chunk) = serde_json::from_slice::<StreamChunk>(&buf) {
            if let Some(msg) = chunk.message {
                if !msg.content.is_empty() {
                    on_chunk(&msg.content);
                    assistant.push_str(&msg.content);
                }
            }
        }
    }
    Ok(assistant)
}
