//! End-to-end test: a Copilot prompt turn emits a stable ACP `usage_update`
//! carrying both context usage and usage-based (AIU) cost, all sourced from
//! upstream Copilot data.
//!
//! Flow exercised:
//!   1. The provider resolves a GitHub token from the env fallback
//!      (`COPILOT_GITHUB_TOKEN`), then hits the token-exchange endpoint —
//!      which the mock 404s (as happens for PATs), so it falls back to the
//!      direct-token path against `COPILOT_BASE_URL`.
//!   2. `session/new` lists models via `GET /models`; the mock advertises a
//!      reasoning-capable model with a
//!      `capabilities.limits.max_context_window_tokens`.
//!   3. `session/prompt` streams `POST /chat/completions`; the final SSE chunk
//!      carries `usage` and `copilot_usage.total_nano_aiu` (both requested via
//!      `stream_options.include_usage`).
//!   4. The provider emits a WIT `usage-update`; the host forwards it as a
//!      `session/update` with `sessionUpdate: "usage_update"`, `used` =
//!      `total_tokens`, `size` = the model's context window, and `cost` = the
//!      turn's AI-Unit consumption (`total_nano_aiu` / 1e9, currency `AIU`).

mod common;

use common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal mock of the Copilot API surface the provider touches.
struct CopilotMock {
    server: MockServer,
}

impl CopilotMock {
    async fn start() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    fn base_url(&self) -> String {
        self.server.uri()
    }

    fn token_url(&self) -> String {
        format!("{}/copilot_internal/v2/token", self.server.uri())
    }

    /// The token-exchange endpoint 404s (as it does for fine-grained PATs and
    /// gh-CLI tokens), forcing the provider onto the direct-token path.
    async fn expect_token_exchange_404(&self) {
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&self.server)
            .await;
    }

    /// `GET /models` advertising a single reasoning-capable chat model with a
    /// context-window limit. (The real API carries no `billing` field — cost
    /// comes from the chat stream, not the model entry.)
    async fn expect_models(&self, id: &str, context_window: u64) {
        let body = json!({
            "data": [{
                "id": id,
                "name": id,
                "model_picker_category": "powerful",
                "capabilities": {
                    "type": "chat",
                    "supports": { "reasoning_effort": ["low", "medium", "high"] },
                    "limits": { "max_context_window_tokens": context_window }
                }
            }]
        });
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// `POST /chat/completions` streaming a short SSE response whose final
    /// chunk carries token accounting and usage-based (AIU) billing.
    async fn expect_chat_with_usage(
        &self,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        total_nano_aiu: u64,
    ) {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"role":"assistant","content":content},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens
            },"copilot_usage":{
                "total_nano_aiu": total_nano_aiu
            }}),
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&self.server)
            .await;
    }
}

#[tokio::test]
async fn copilot_prompt_emits_usage_update_with_cost() {
    ensure_artifacts();
    let copilot_wasm = copilot_provider_wasm();
    assert!(
        copilot_wasm.exists(),
        "missing artifact: {}\nrun `just build` first.",
        copilot_wasm.display()
    );

    const MODEL: &str = "gpt-5-e2e";
    const CONTEXT_WINDOW: u64 = 128_000;
    const PROMPT_TOKENS: u64 = 100;
    const COMPLETION_TOKENS: u64 = 20;
    const TOTAL_TOKENS: u64 = 120;
    const TOTAL_NANO_AIU: u64 = 39_000_000;
    const EXPECTED_AIU: f64 = 0.039;

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models(MODEL, CONTEXT_WINDOW).await;
    copilot
        .expect_chat_with_usage(
            "Hi there!",
            PROMPT_TOKENS,
            COMPLETION_TOKENS,
            TOTAL_TOKENS,
            TOTAL_NANO_AIU,
        )
        .await;

    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .provider(copilot_provider_wasm())
        .with_keyring_store("mock")
        .env("COPILOT_GITHUB_TOKEN", "gho_e2e_faketoken")
        .env("COPILOT_BASE_URL", copilot.base_url())
        .env("COPILOT_TOKEN_URL", copilot.token_url())
        .env("COPILOT_MODEL", MODEL)
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

    let resp = host
        .request("session/prompt", prompt_text_params(&session_id, "hi"))
        .await
        .unwrap();
    assert!(resp.is_object(), "prompt response: {resp}");

    // The provider emits the context-usage meter twice per turn: once at the
    // start (so the UI is stable the instant prompting begins) carrying the
    // last-known `used` — `0` for a fresh session — and no cost yet, then again
    // at the end with this turn's real token + AIU figures. Collect both, and
    // track whether any agent text arrived before the first meter — it must
    // not, or the UI would still shift once the meter appears mid-response.
    let mut usages: Vec<Value> = Vec::new();
    let mut agent_text_before_first_meter = false;
    while let Ok(msg) = host.recv_any().await {
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = msg.pointer("/params/update").cloned().unwrap_or(Value::Null);
            match update.get("sessionUpdate").and_then(Value::as_str) {
                Some("agent_message_chunk") if usages.is_empty() => {
                    agent_text_before_first_meter = true;
                }
                Some("usage_update") => {
                    let is_final =
                        update.get("used").and_then(Value::as_u64) == Some(TOTAL_TOKENS);
                    usages.push(update);
                    if is_final {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    assert!(
        usages.len() >= 2,
        "expected a start-of-turn and an end-of-turn usage_update, got: {usages:?}"
    );
    assert!(
        !agent_text_before_first_meter,
        "the usage meter must be emitted before any agent text so the UI doesn't shift mid-turn"
    );

    // Start-of-turn meter: present from the first instant, at `0` used against
    // the real context window, with no cost yet for a fresh session.
    let start = &usages[0];
    assert_eq!(
        start.get("used").and_then(Value::as_u64),
        Some(0),
        "start-of-turn usage_update.used should be 0 for a fresh session (update: {start})"
    );
    assert_eq!(
        start.get("size").and_then(Value::as_u64),
        Some(CONTEXT_WINDOW),
        "start-of-turn usage_update.size should be the model's context window (update: {start})"
    );
    assert!(
        start.get("cost").map(Value::is_null).unwrap_or(true),
        "start-of-turn usage_update should carry no cost yet (update: {start})"
    );

    // End-of-turn meter: refreshed in place with this turn's real figures.
    let usage = usages.last().unwrap();
    assert_eq!(
        usage.get("used").and_then(Value::as_u64),
        Some(TOTAL_TOKENS),
        "usage_update.used should be the chat response's total_tokens (full update: {usage})"
    );
    assert_eq!(
        usage.get("size").and_then(Value::as_u64),
        Some(CONTEXT_WINDOW),
        "usage_update.size should be the model's context window (full update: {usage})"
    );
    assert_eq!(
        usage.pointer("/cost/amount").and_then(Value::as_f64),
        Some(EXPECTED_AIU),
        "usage_update.cost.amount should be total_nano_aiu / 1e9 (full update: {usage})"
    );
    assert_eq!(
        usage.pointer("/cost/currency").and_then(Value::as_str),
        Some("AIU"),
        "usage_update.cost.currency should be AIU (full update: {usage})"
    );

    // Continuing session: a second turn's *start* meter must carry the
    // last-known figures (persisted from turn 1) rather than flashing back to
    // `0`/no-cost — this is what keeps the UI stable across turns, not just at
    // the very first prompt.
    let resp2 = host
        .request("session/prompt", prompt_text_params(&session_id, "again"))
        .await
        .unwrap();
    assert!(resp2.is_object(), "second prompt response: {resp2}");

    let mut start2: Option<Value> = None;
    while let Ok(msg) = host.recv_any().await {
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = msg.pointer("/params/update").cloned().unwrap_or(Value::Null);
            if update.get("sessionUpdate").and_then(Value::as_str) == Some("usage_update") {
                start2 = Some(update);
                break;
            }
        }
    }
    let start2 = start2.expect("expected a start-of-turn usage_update on the second turn");
    assert_eq!(
        start2.get("used").and_then(Value::as_u64),
        Some(TOTAL_TOKENS),
        "2nd turn's start meter should carry turn 1's persisted `used` (update: {start2})"
    );
    assert_eq!(
        start2.pointer("/cost/amount").and_then(Value::as_f64),
        Some(EXPECTED_AIU),
        "2nd turn's start meter should carry turn 1's persisted cost (update: {start2})"
    );
    assert_eq!(
        start2.pointer("/cost/currency").and_then(Value::as_str),
        Some("AIU"),
        "2nd turn's start meter cost currency should be AIU (update: {start2})"
    );
}
