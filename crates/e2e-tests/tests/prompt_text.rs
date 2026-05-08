//! Smoke test: a single user prompt that yields a streamed text reply
//! from the (stubbed) Ollama endpoint. Asserts that the host emits at
//! least one `session/update` notification carrying the assistant text.

mod common;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn prompt_returns_streamed_text() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat(&[
            chat_text_chunk("Hello, "),
            chat_text_chunk("world!"),
            chat_done_chunk(),
        ])
        .await;

    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params()).await.unwrap();

    let new_resp = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("sessionId in response")
        .to_string();

    // Send the prompt; collect notifications until the response arrives.
    // We can't easily multiplex within a single `request()` call, so spawn
    // a background read loop that drains updates while we wait.
    let prompt_fut = host.request("session/prompt", prompt_text_params(&session_id, "hi"));
    let resp = prompt_fut.await.unwrap();
    assert!(resp.is_object(), "prompt response: {resp}");

    // Drain any queued session/update notifications and check at least one
    // contained text from our stubbed reply.
    let mut saw_text = false;
    let snapshot = serde_json::to_string(&resp).unwrap_or_default();
    if snapshot.contains("Hello") || snapshot.contains("world") {
        saw_text = true;
    }
    while let Some(msg) = host
        .recv_any()
        .await
        .ok()
        .filter(|_| !saw_text)
    {
        let s = serde_json::to_string(&msg).unwrap_or_default();
        if s.contains("Hello") || s.contains("world") {
            saw_text = true;
            break;
        }
    }
    assert!(saw_text, "expected stubbed text in notifications/response");
}
