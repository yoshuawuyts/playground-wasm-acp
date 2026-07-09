//! End-to-end test: a prompt turn emits a stable ACP `usage_update`
//! session notification carrying context-window usage.
//!
//! Exercises the full context-% path added alongside the
//! agent-client-protocol 1.2.0 upgrade:
//!   - Ollama's `/api/chat` final `done` chunk reports
//!     `prompt_eval_count` + `eval_count`.
//!   - The provider fetches the model's context window from `/api/show`
//!     (`model_info.*.context_length`) and emits a WIT `usage-update`.
//!   - The host translates it to schema v1 `SessionUpdate::UsageUpdate`
//!     and forwards it as a `session/update` notification with
//!     `sessionUpdate: "usage_update"`.
//!
//! `used` = prompt + generated tokens (tokens now occupying context);
//! `size` = the model's context-window length. Editors render the
//! context-% indicator as `used / size`.

mod common;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn prompt_emits_usage_update() {
    ensure_artifacts();

    const PROMPT_EVAL: u64 = 100;
    const EVAL: u64 = 20;
    const CONTEXT_LENGTH: u64 = 8192;

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama
        .expect_show_with_tools_and_context(CONTEXT_LENGTH)
        .await;
    ollama
        .expect_chat(&[
            chat_text_chunk("Hello!"),
            chat_done_chunk_with_usage(PROMPT_EVAL, EVAL),
        ])
        .await;

    let cwd = tempfile::tempdir().unwrap();

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
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("sessionId in response")
        .to_string();

    // The `usage_update` is emitted after the chat loop settles but before
    // the prompt response resolves, so `request` buffers it; we drain the
    // buffered `session/update` notifications afterward.
    let resp = host
        .request("session/prompt", prompt_text_params(&session_id, "hi"))
        .await
        .unwrap();
    assert!(resp.is_object(), "prompt response: {resp}");

    // Scan the queued notifications for the usage_update.
    let mut usage: Option<Value> = None;
    while let Ok(msg) = host.recv_any().await {
        let is_update = msg.get("method").and_then(Value::as_str) == Some("session/update");
        if is_update {
            let update = msg.pointer("/params/update").cloned().unwrap_or(Value::Null);
            if update.get("sessionUpdate").and_then(Value::as_str) == Some("usage_update") {
                usage = Some(update);
                break;
            }
        }
    }

    let usage = usage.expect("expected a session/update with sessionUpdate=usage_update");
    assert_eq!(
        usage.get("used").and_then(Value::as_u64),
        Some(PROMPT_EVAL + EVAL),
        "usage_update.used should be prompt_eval_count + eval_count (full update: {usage})"
    );
    assert_eq!(
        usage.get("size").and_then(Value::as_u64),
        Some(CONTEXT_LENGTH),
        "usage_update.size should be the model context window (full update: {usage})"
    );
}
