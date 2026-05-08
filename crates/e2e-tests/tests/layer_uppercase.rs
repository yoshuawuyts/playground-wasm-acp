//! Smoke test: layered chain. Wraps the ollama provider in the
//! `uppercase-layer` and verifies the layer's text rewrite is observed
//! by the host (i.e. the assistant text reaches the editor uppercased).

mod common;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn uppercase_layer_rewrites_text() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat(&[
            chat_text_chunk("hello from ollama"),
            chat_done_chunk(),
        ])
        .await;

    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .with_layer(layer_wasm())
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
        .unwrap()
        .to_string();

    let prompt_id: i64 = 200;
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "say hi")
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();

    let mut saw_uppercased = false;
    loop {
        let msg = host.recv_any().await.unwrap();
        let s = serde_json::to_string(&msg).unwrap_or_default();
        if s.contains("HELLO FROM OLLAMA") {
            saw_uppercased = true;
        }
        if msg.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            break;
        }
    }
    assert!(
        saw_uppercased,
        "expected an uppercased text fragment (HELLO FROM OLLAMA) in stream",
    );
}
