//! End-to-end test: the Copilot provider remembers the last-selected model
//! (and thinking level) across sessions, so a brand-new session's **Thinking**
//! (reasoning-effort) selector is populated **from the start** whenever the
//! last-used model supports reasoning — instead of only appearing after the
//! user switches away from a non-reasoning default (e.g. `gpt-4o`).
//!
//! Flow exercised:
//!   1. `session/new` (session A) defaults to `COPILOT_MODEL` — a model with
//!      **no** upstream `reasoning_effort` — so its config options contain
//!      only the `model` selector, no `reasoning-effort`.
//!   2. `session/set_config_option` switches session A to a reasoning-capable
//!      model; the response now includes the `reasoning-effort` selector, and
//!      the choice is persisted to `/data/preferences.json`.
//!   3. `session/new` (session B, same cwd → same `/data`) inherits the
//!      persisted model, so its config options include `reasoning-effort`
//!      from the start.

mod common;

use common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NON_REASONING_MODEL: &str = "plain-model-e2e";
const REASONING_MODEL: &str = "thinky-model-e2e";
const CONTEXT_WINDOW: u64 = 128_000;

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

    async fn expect_token_exchange_404(&self) {
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&self.server)
            .await;
    }

    /// `GET /models` advertising two chat models: one without reasoning
    /// levels (the default) and one with them.
    async fn expect_models(&self) {
        let body = json!({
            "data": [
                {
                    "id": NON_REASONING_MODEL,
                    "name": NON_REASONING_MODEL,
                    "capabilities": {
                        "type": "chat",
                        "limits": { "max_context_window_tokens": CONTEXT_WINDOW }
                    }
                },
                {
                    "id": REASONING_MODEL,
                    "name": REASONING_MODEL,
                    "model_picker_category": "powerful",
                    "capabilities": {
                        "type": "chat",
                        "supports": { "reasoning_effort": ["low", "medium", "high"] },
                        "limits": { "max_context_window_tokens": CONTEXT_WINDOW }
                    }
                }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }
}

/// Collect the `id`s of the config options in a `session/new` or
/// `session/set_config_option` response.
fn option_ids(resp: &Value) -> Vec<String> {
    resp.get("configOptions")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The `currentValue` of the option with the given `id`, if present.
fn current_value(resp: &Value, id: &str) -> Option<String> {
    resp.get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|o| o.get("currentValue").and_then(Value::as_str))
        .map(str::to_string)
}

#[tokio::test]
async fn thinking_selector_persists_to_new_sessions() {
    ensure_artifacts();
    let copilot_wasm = copilot_provider_wasm();
    assert!(
        copilot_wasm.exists(),
        "missing artifact: {}\nrun `just build` first.",
        copilot_wasm.display()
    );

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;

    // A fresh state root so no preferences carry over from prior runs, and a
    // single cwd shared by both sessions so they map to the same `/data`.
    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .provider(copilot_provider_wasm())
        .with_keyring_store("mock")
        .env("COPILOT_GITHUB_TOKEN", "gho_e2e_faketoken")
        .env("COPILOT_BASE_URL", copilot.base_url())
        .env("COPILOT_TOKEN_URL", copilot.token_url())
        .env("COPILOT_MODEL", NON_REASONING_MODEL)
        .env("XDG_STATE_HOME", state_home.path().to_str().unwrap())
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params())
        .await
        .unwrap();

    // Session A: defaults to the non-reasoning model → no Thinking selector.
    let a = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let a_id = a
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("sessionId")
        .to_string();
    let ids = option_ids(&a);
    assert!(
        ids.contains(&"model".to_string()),
        "session A should offer a model selector: {a}"
    );
    assert!(
        !ids.contains(&"reasoning-effort".to_string()),
        "session A defaults to a non-reasoning model, so it must NOT yet offer a \
         Thinking selector: {a}"
    );
    assert_eq!(
        current_value(&a, "model").as_deref(),
        Some(NON_REASONING_MODEL),
        "session A should start on the configured default model: {a}"
    );

    // Switch session A to the reasoning-capable model.
    let switched = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": a_id, "configId": "model", "value": REASONING_MODEL }),
        )
        .await
        .unwrap();
    assert!(
        option_ids(&switched).contains(&"reasoning-effort".to_string()),
        "after switching to a reasoning model, the Thinking selector should appear: {switched}"
    );

    // Session B (same cwd → same /data): inherits the persisted model, so the
    // Thinking selector is present from the start.
    let b = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let ids = option_ids(&b);
    assert!(
        ids.contains(&"reasoning-effort".to_string()),
        "a new session must inherit the last-used reasoning model and offer the \
         Thinking selector from the start: {b}"
    );
    assert_eq!(
        current_value(&b, "model").as_deref(),
        Some(REASONING_MODEL),
        "a new session should default to the last-selected model: {b}"
    );
    assert_eq!(
        current_value(&b, "reasoning-effort").as_deref(),
        Some("medium"),
        "the inherited thinking level should be the persisted default: {b}"
    );
}
