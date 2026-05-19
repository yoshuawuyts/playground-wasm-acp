//! End-to-end test for the layer's `/shout` slash command.
//!
//! Verifies three things:
//!   1. After `session/new`, the layer's `available-commands-update`
//!      notification reaches the editor (the gate flushes it after the
//!      response goes out).
//!   2. Sending `/shout` toggles the layer's uppercase mode — the
//!      acknowledgement comes back uppercased and Ollama is **not**
//!      contacted (no chat call).
//!   3. A subsequent regular prompt streams uppercased text through.

mod common;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn shout_toggles_layer_uppercase() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    // Only one chat exchange is expected: after `/shout` enables
    // uppercasing, the *second* user prompt ("hello") drives Ollama.
    ollama
        .expect_chat(&[chat_text_chunk("hello from ollama"), chat_done_chunk()])
        .await;

    let cwd = tempfile::tempdir().unwrap();
    let mut host = HostBuilder::new()
        .with_layer(layer_wasm())
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params())
        .await
        .unwrap();

    // Drive `session/new` *manually* with a chosen id so we can observe
    // the on-wire order of the response and the
    // `available_commands_update` notification. Zed is order-sensitive:
    // a notification arriving before the editor has registered the
    // session id is silently dropped, leading to "Available commands:
    // none" even though the layer advertised them.
    let new_id: i64 = 50;
    let new_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": new_id,
        "method": "session/new",
        "params": new_session_params(cwd.path()),
    });
    host.raw_write(&serde_json::to_string(&new_req).unwrap())
        .await
        .unwrap();

    let mut session_id = String::new();
    let mut saw_response = false;
    let mut saw_commands = false;
    let mut response_first = false;
    loop {
        let msg = host.recv_any().await.unwrap();
        if msg.get("id").and_then(Value::as_i64) == Some(new_id) {
            assert!(msg.get("error").is_none(), "session/new errored: {msg}");
            session_id = msg
                .pointer("/result/sessionId")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            saw_response = true;
            // The available-commands notification MUST arrive after
            // the session/new response, so we shouldn't have seen it
            // yet at this point.
            response_first = !saw_commands;
            continue;
        }
        if msg.get("method").and_then(Value::as_str) == Some("session/update")
            && msg
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("available_commands_update")
        {
            let cmds = msg
                .pointer("/params/update/availableCommands")
                .and_then(Value::as_array)
                .unwrap();
            // The host emits a synthetic `/install`-only advertisement
            // before flushing the layer's own commands update; skip
            // updates that don't carry `/shout` and keep waiting.
            let has_shout = cmds
                .iter()
                .any(|c| c.get("name").and_then(Value::as_str) == Some("shout"));
            if !has_shout {
                continue;
            }
            saw_commands = true;
            if saw_response {
                break;
            }
        }
    }
    assert!(saw_response && saw_commands);
    assert!(
        response_first,
        "available_commands_update arrived BEFORE session/new response — Zed will drop it"
    );

    // (2) Send `/shout` — layer should ack uppercased and NOT call Ollama.
    let shout_id: i64 = 100;
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": shout_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "/shout"),
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();
    let mut saw_ack = false;
    loop {
        let msg = host.recv_any().await.unwrap();
        let s = serde_json::to_string(&msg).unwrap_or_default();
        if s.contains("CAPS LOCK ENGAGED!") {
            saw_ack = true;
        }
        if msg.get("id").and_then(Value::as_i64) == Some(shout_id) {
            assert!(msg.get("error").is_none(), "/shout errored: {msg}");
            break;
        }
    }
    assert!(saw_ack, "expected the layer's `/shout` ack chunk");

    // (3) A regular prompt must now stream uppercased text from Ollama.
    let prompt_id: i64 = 200;
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "say hi"),
    });
    host.raw_write(&serde_json::to_string(&req).unwrap())
        .await
        .unwrap();

    let mut saw_uppercased = false;
    let mut saw_lowercased = false;
    loop {
        let msg = host.recv_any().await.unwrap();
        // Inspect only the text of agent_message_chunk updates so we
        // don't get false positives from session ids, method names, etc.
        if msg.get("method").and_then(Value::as_str) == Some("session/update")
            && msg
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("agent_message_chunk")
            && let Some(text) = msg
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
        {
            if text.chars().any(|c| c.is_ascii_lowercase()) {
                saw_lowercased = true;
            }
            // A chunk that contains ≥1 ASCII letter and no lowercase
            // letters is evidence that the layer is rewriting agent
            // output for this session.
            if text.chars().any(|c| c.is_ascii_uppercase())
                && !text.chars().any(|c| c.is_ascii_lowercase())
            {
                saw_uppercased = true;
            }
        }
        if msg.get("id").and_then(Value::as_i64) == Some(prompt_id) {
            break;
        }
    }
    assert!(
        !saw_lowercased,
        "expected layer to rewrite text, but lowercase fragment leaked through",
    );
    assert!(
        saw_uppercased,
        "expected at least one uppercased agent_message_chunk in the post-/shout stream",
    );
}
