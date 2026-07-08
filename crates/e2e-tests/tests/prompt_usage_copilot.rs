//! End-to-end test: a Copilot prompt turn emits a stable ACP `usage_update`
//! carrying both context usage and premium-request cost, all sourced from
//! upstream Copilot data.
//!
//! Flow exercised:
//!   1. The provider resolves a GitHub token from the env fallback
//!      (`COPILOT_GITHUB_TOKEN`), then hits the token-exchange endpoint —
//!      which the mock 404s (as happens for PATs), so it falls back to the
//!      direct-token path against `COPILOT_BASE_URL`.
//!   2. `session/new` lists models via `GET /models`; the mock advertises a
//!      premium model with `billing` ({is_premium, multiplier}) and a
//!      `capabilities.limits.max_context_window_tokens`.
//!   3. `session/prompt` streams `POST /chat/completions`; the final SSE chunk
//!      carries `usage` (requested via `stream_options.include_usage`).
//!   4. The provider emits a WIT `usage-update`; the host forwards it as a
//!      `session/update` with `sessionUpdate: "usage_update"`, `used` =
//!      `total_tokens`, `size` = the model's context window, and `cost` = the
//!      per-turn premium-request consumption (multiplier of the premium model).

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

    /// `GET /models` advertising a single premium, reasoning-capable chat
    /// model with a context-window limit.
    async fn expect_models(&self, id: &str, context_window: u64, multiplier: f64) {
        let body = json!({
            "data": [{
                "id": id,
                "name": id,
                "capabilities": {
                    "type": "chat",
                    "supports": { "reasoning_effort": ["low", "medium", "high"] },
                    "limits": { "max_context_window_tokens": context_window }
                },
                "billing": { "is_premium": true, "multiplier": multiplier }
            }]
        });
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// `POST /chat/completions` streaming a short SSE response whose final
    /// chunk carries token accounting.
    async fn expect_chat_with_usage(
        &self,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    ) {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"role":"assistant","content":content},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens
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
    const MULTIPLIER: f64 = 1.0;
    const PROMPT_TOKENS: u64 = 100;
    const COMPLETION_TOKENS: u64 = 20;
    const TOTAL_TOKENS: u64 = 120;

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot
        .expect_models(MODEL, CONTEXT_WINDOW, MULTIPLIER)
        .await;
    copilot
        .expect_chat_with_usage("Hi there!", PROMPT_TOKENS, COMPLETION_TOKENS, TOTAL_TOKENS)
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

    // Drain buffered notifications for the usage_update.
    let mut usage: Option<Value> = None;
    while let Ok(msg) = host.recv_any().await {
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
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
        Some(MULTIPLIER),
        "usage_update.cost.amount should be the premium model's multiplier (full update: {usage})"
    );
    assert_eq!(
        usage.pointer("/cost/currency").and_then(Value::as_str),
        Some("premium-requests"),
        "usage_update.cost.currency should be premium-requests (full update: {usage})"
    );
}
