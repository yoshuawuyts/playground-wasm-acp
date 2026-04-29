//! Minimal HTTP client for the Ollama `/api/chat` endpoint.

use serde::{Deserialize, Serialize};
use wstd::http::{Body, Client, Method, Request};

const OLLAMA_URL: &str = "http://localhost:11434/api/chat";
const DEFAULT_MODEL: &str = "llama3.2";

#[derive(Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<ChatMessage<'a>>,
    pub stream: bool,
}

#[derive(Serialize)]
pub struct ChatMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

#[derive(Deserialize)]
pub struct ChatResponse {
    pub message: ResponseMessage,
}

#[derive(Deserialize)]
pub struct ResponseMessage {
    pub content: String,
}

/// Send a single non-streaming chat completion to Ollama and return the
/// assistant's reply text.
pub async fn chat(user_text: &str) -> Result<String, String> {
    let body = Body::from_json(&ChatRequest {
        model: DEFAULT_MODEL,
        messages: vec![ChatMessage {
            role: "user",
            content: user_text,
        }],
        stream: false,
    })
    .map_err(|e| format!("encode: {e}"))?;

    let req = Request::builder()
        .method(Method::POST)
        .uri(OLLAMA_URL)
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
    let parsed: ChatResponse = resp
        .body_mut()
        .json()
        .await
        .map_err(|e| format!("decode: {e}"))?;
    Ok(parsed.message.content)
}
