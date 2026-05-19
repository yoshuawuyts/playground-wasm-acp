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

    host.request("initialize", initialize_params())
        .await
        .unwrap();
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

    let (req_id, params) = host
        .wait_inbound_request("fs/read_text_file")
        .await
        .unwrap();
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

/// Same shape as the round-trip test, but the model returns a *relative*
/// path (`hello.txt`) which the provider must resolve against the
/// session's cwd before issuing `fs/read_text_file`. This is the
/// realistic path: local Ollama models almost never emit absolute paths.
#[tokio::test]
async fn prompt_with_tool_call_relative_path() {
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
                chat_tool_chunk("read_file", json!({"path": "hello.txt"})),
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

    host.request("initialize", initialize_params())
        .await
        .unwrap();
    let new_resp = host
        .request("session/new", new_session_params(dir.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let prompt_id: i64 = 200;
    let req = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "read hello.txt")
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();

    let (req_id, params) = host
        .wait_inbound_request("fs/read_text_file")
        .await
        .unwrap();
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    assert!(
        path.ends_with("/hello.txt") && path.starts_with('/'),
        "expected resolved absolute path, got {path:?}"
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

/// The editor reports a failed `fs/read_text_file` (e.g. file missing /
/// permission denied). The provider must surface this as a *failed*
/// tool-call update and still feed the error back into the model so the
/// final prompt response completes successfully (no JSON-RPC error on
/// the prompt request).
#[tokio::test]
async fn prompt_with_tool_call_fs_error() {
    ensure_artifacts();

    let dir = tempfile::tempdir().unwrap();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat_sequence(&[
            vec![
                chat_tool_chunk("read_file", json!({"path": "missing.txt"})),
                chat_done_chunk(),
            ],
            vec![
                chat_text_chunk("Sorry, I couldn't read that."),
                chat_done_chunk(),
            ],
        ])
        .await;

    let mut host = HostBuilder::new()
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params())
        .await
        .unwrap();
    let new_resp = host
        .request("session/new", new_session_params(dir.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let prompt_id: i64 = 300;
    let req = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "read missing.txt")
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();

    let (req_id, _params) = host
        .wait_inbound_request("fs/read_text_file")
        .await
        .unwrap();
    // Simulate the editor responding with a JSON-RPC error.
    let err_resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": -32603, "message": "ENOENT: missing.txt"}
    });
    host.raw_write(&serde_json::to_string(&err_resp).unwrap())
        .await
        .unwrap();

    loop {
        let msg = host.recv_any().await.unwrap();
        if msg.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            assert!(
                msg.get("error").is_none(),
                "prompt errored instead of recovering from fs error: {msg}"
            );
            return;
        }
    }
}
