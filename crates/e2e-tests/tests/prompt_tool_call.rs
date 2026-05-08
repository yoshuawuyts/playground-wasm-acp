//! Smoke test: tool-call round-trip. The first stubbed `/api/chat`
//! response asks the model to call `read_file`; the host bridges that
//! to an `fs/read_text_file` request which this test answers; the second
//! stubbed `/api/chat` response then yields the final assistant text.

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::Write;

#[tokio::test]
async fn prompt_with_tool_call_round_trip() {
    ensure_artifacts();

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("hello.txt");
    {
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "FILE_CONTENTS_42").unwrap();
    }

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat_sequence(&[
            vec![
                chat_tool_chunk("read_file", json!({"path": file_path.to_string_lossy()})),
                chat_done_chunk(),
            ],
            vec![chat_text_chunk("Got it: 42"), chat_done_chunk()],
        ])
        .await;

    let mut host = HostBuilder::new()
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params()).await.unwrap();
    let new_resp = host
        .request("session/new", new_session_params(dir.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Send the prompt with a fixed id so we can service the inbound
    // fs/read_text_file request before awaiting the response.
    let prompt_id: i64 = 100;
    let req = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "read it")
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();

    let (req_id, params) = host.wait_inbound_request("fs/read_text_file").await.unwrap();
    assert!(
        params
            .get("path")
            .and_then(Value::as_str)
            .map(|p| p.ends_with("hello.txt"))
            .unwrap_or(false),
        "unexpected fs/read_text_file params: {params}"
    );
    host.respond(req_id, json!({"content": "FILE_CONTENTS_42\n"}))
        .await
        .unwrap();

    loop {
        let msg = host.recv_any().await.unwrap();
        if msg.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            assert!(msg.get("error").is_none(), "prompt errored: {msg}");
            return;
        }
    }
}
