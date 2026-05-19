//! End-to-end test: cancel an in-flight prompt and verify it returns
//! with `stop_reason: cancelled`.
//!
//! Exercises:
//!   - Phase 1 cancel watch race in [`crate::wasm::Session::prompt`].
//!   - Phase 2 `session.cancel` resource method dispatched through a
//!     layer: the host's `HostSessionWithStore::cancel` looks up the
//!     downstream `ResourceAny` in [`HostState::downstream_sessions`]
//!     and forwards to the provider's exported `session.cancel`.
//!
//! Setup: stub Ollama with a slow `/api/chat` so the prompt sits
//! mid-call long enough for us to send `session/cancel`. We run the
//! provider behind the `uppercase-layer` so the cancel notification
//! traverses the layer's `LayerSession::cancel` → downstream forward
//! path.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn prompt_can_be_cancelled_mid_flight() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;

    // Mount the chat endpoint with a long delay so the wasm prompt
    // future is genuinely in flight while we send the cancel. The
    // delay is much longer than the test should ever need to send the
    // cancel notification.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let body = ndjson(&[
        chat_text_chunk("this should never arrive"),
        chat_done_chunk(),
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_raw(body, "application/x-ndjson")
                .set_delay(Duration::from_secs(30)),
        )
        .mount(ollama.server())
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
    let new_resp = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let session_id = new_resp
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Fire the prompt without awaiting its response.
    let prompt_id: i64 = 200;
    let prompt_req = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": prompt_text_params(&session_id, "go slow"),
    });
    host.raw_write(&serde_json::to_string(&prompt_req).unwrap())
        .await
        .unwrap();

    // Give the prompt a moment to reach the wasm chain — without this
    // the cancel can race ahead and the watch is already cleared by
    // the start-of-prompt `send_replace(false)`.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send the cancel notification (one-way, no response).
    host.notify("session/cancel", json!({ "sessionId": session_id }))
        .await
        .unwrap();

    // Drain messages until the prompt response arrives. Bound the wait
    // so a hung cancel doesn't keep the test alive for the 30 s mock
    // delay.
    let resp = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = host.recv_any().await.unwrap();
            if msg.get("id").and_then(Value::as_i64) == Some(prompt_id) {
                break msg;
            }
        }
    })
    .await
    .expect("prompt response should arrive within 5s of cancel");

    assert!(
        resp.get("error").is_none(),
        "prompt response carried an error: {resp}"
    );
    let stop_reason = resp
        .pointer("/result/stopReason")
        .and_then(Value::as_str)
        .expect("prompt response missing stopReason");
    assert_eq!(
        stop_reason, "cancelled",
        "expected stop_reason=cancelled, got {stop_reason} (full response: {resp})"
    );
}
