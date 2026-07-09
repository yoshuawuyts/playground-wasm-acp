//! End-to-end test: the host loads **two** providers at once (ollama +
//! copilot) and merges their model selectors into one cross-provider
//! `model` dropdown. Each entry is labelled by the provider that owns it.
//!
//! The provider owning the *selected* model is the **active** provider: it
//! backs prompts and the non-model selectors. Selecting a model from the
//! other provider switches the active provider — which we observe by the
//! copilot-only `mode` / `allow-all` selectors appearing (and disappearing
//! again when switching back to an ollama model). A final prompt confirms
//! the turn routes to the active (ollama) provider.

mod common;

use common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OLLAMA_MODEL_A: &str = "ollama-model-a";
const OLLAMA_MODEL_B: &str = "ollama-model-b";
const COPILOT_MODEL: &str = "copilot-e2e-model";
const CONTEXT_WINDOW: u64 = 128_000;

const OLLAMA_PROVIDER_ID: &str = "local:ollama_provider";
const COPILOT_PROVIDER_ID: &str = "local:copilot_provider";

// ---------------------------------------------------------------------------
// Minimal Copilot HTTP mock (models list + token exchange).
// ---------------------------------------------------------------------------

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
    async fn expect_models(&self) {
        let body = json!({
            "data": [{
                "id": COPILOT_MODEL,
                "name": COPILOT_MODEL,
                "capabilities": {
                    "type": "chat",
                    "limits": { "max_context_window_tokens": CONTEXT_WINDOW }
                }
            }]
        });
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }
    /// `POST /chat/completions` streaming a short SSE response so a prompt
    /// routed to the copilot provider produces streamed assistant text.
    async fn expect_chat(&self, content: &str) {
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"role":"assistant","content":content},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
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

// ---------------------------------------------------------------------------
// Helpers for reading the merged `model` selector.
// ---------------------------------------------------------------------------

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

fn model_option(resp: &Value) -> &Value {
    resp.get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
        })
        .unwrap_or_else(|| panic!("no `model` config option in: {resp}"))
}

fn model_current_value(resp: &Value) -> String {
    model_option(resp)
        .get("currentValue")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("model option has no currentValue: {resp}"))
        .to_string()
}

/// The merged model selector's native groups as
/// `(group_id, group_name, [(value, name)])` — one per provider.
fn model_groups(resp: &Value) -> Vec<(String, String, Vec<(String, String)>)> {
    model_option(resp)
        .get("options")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .map(|g| {
                    let gid = g
                        .get("group")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let gname = g
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let opts = g
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|os| {
                            os.iter()
                                .map(|o| {
                                    (
                                        o.get("value")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        o.get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (gid, gname, opts)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `(value, name)` for every entry across all groups in the merged model
/// selector.
fn model_choices(resp: &Value) -> Vec<(String, String)> {
    model_groups(resp)
        .into_iter()
        .flat_map(|(_, _, opts)| opts)
        .collect()
}

/// The `value` of the first model entry in the group whose id is
/// `provider_id`. Panics if no such group exists.
fn value_for_provider(resp: &Value, provider_id: &str) -> String {
    model_groups(resp)
        .into_iter()
        .find(|(gid, _, _)| gid == provider_id)
        .and_then(|(_, _, opts)| opts.into_iter().next().map(|(value, _)| value))
        .unwrap_or_else(|| panic!("no model group `{provider_id}` in: {resp}"))
}

/// The id of the group that owns the model entry with `value`.
fn group_for_value(resp: &Value, value: &str) -> String {
    model_groups(resp)
        .into_iter()
        .find(|(_, _, opts)| opts.iter().any(|(v, _)| v == value))
        .map(|(gid, _, _)| gid)
        .unwrap_or_else(|| panic!("no group owns model value `{value}` in: {resp}"))
}

#[tokio::test]
async fn merges_models_across_providers_and_switches_active() {
    ensure_artifacts();
    assert!(
        copilot_provider_wasm().exists(),
        "missing copilot artifact; run `just build` first"
    );

    // Upstream mocks: one per provider.
    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&[OLLAMA_MODEL_A, OLLAMA_MODEL_B]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat(&[
            chat_text_chunk("Hello, "),
            chat_text_chunk("world!"),
            chat_done_chunk(),
        ])
        .await;

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;
    copilot.expect_chat("Copilot speaking.").await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    // Load ollama first (starts active) and copilot second.
    let mut host = HostBuilder::new()
        .provider(provider_wasm())
        .with_provider(copilot_provider_wasm())
        .with_keyring_store("mock")
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", OLLAMA_MODEL_A)
        .env("COPILOT_GITHUB_TOKEN", "gho_e2e_faketoken")
        .env("COPILOT_BASE_URL", copilot.base_url())
        .env("COPILOT_TOKEN_URL", copilot.token_url())
        .env("COPILOT_MODEL", COPILOT_MODEL)
        .env("XDG_STATE_HOME", state_home.path().to_str().unwrap())
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params())
        .await
        .unwrap();

    // session/new: one merged model selector spanning both providers.
    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s
        .get("sessionId")
        .and_then(Value::as_str)
        .expect("sessionId")
        .to_string();

    assert_eq!(
        model_option(&s).get("category").and_then(Value::as_str),
        Some("model"),
        "merged selector keeps the model category: {s}"
    );

    // The merged selector groups each provider's models under a native
    // group headed by the provider's component id, with unsuffixed names.
    let groups = model_groups(&s);
    let ollama_group = groups
        .iter()
        .find(|(gid, _, _)| gid == OLLAMA_PROVIDER_ID)
        .unwrap_or_else(|| panic!("no ollama model group: {s}"));
    assert!(
        ollama_group.2.iter().any(|(_, n)| n == OLLAMA_MODEL_A),
        "ollama group must list model A under a clean name: {ollama_group:?}"
    );
    assert!(
        ollama_group.2.iter().any(|(_, n)| n == OLLAMA_MODEL_B),
        "ollama group must list every ollama model: {ollama_group:?}"
    );
    let copilot_group = groups
        .iter()
        .find(|(gid, _, _)| gid == COPILOT_PROVIDER_ID)
        .unwrap_or_else(|| panic!("no copilot model group: {s}"));
    assert!(
        copilot_group.2.iter().any(|(_, n)| n == COPILOT_MODEL),
        "copilot group must list copilot's model: {copilot_group:?}"
    );
    // Native grouping replaces the old `(namespace:component)` suffix, so
    // option names must no longer embed the provider id.
    let names: Vec<String> = model_choices(&s).into_iter().map(|(_, n)| n).collect();
    assert!(
        names
            .iter()
            .all(|n| !n.contains(OLLAMA_PROVIDER_ID) && !n.contains(COPILOT_PROVIDER_ID)),
        "model names must be unsuffixed now that grouping is native: {names:?}"
    );

    // Active provider starts as ollama, so the current model belongs to
    // ollama's group and copilot's mode/allow-all selectors are not shown.
    let current = model_current_value(&s);
    assert_eq!(
        group_for_value(&s, &current),
        OLLAMA_PROVIDER_ID,
        "the initial active model belongs to the first-loaded provider (ollama): {s}"
    );
    let ids = option_ids(&s);
    assert!(
        !ids.contains(&"mode".to_string()) && !ids.contains(&"allow-all".to_string()),
        "while ollama is active, copilot-only selectors must not appear: {ids:?}"
    );

    // Select a copilot model → active switches to copilot, revealing its
    // mode / allow-all selectors.
    let copilot_value = value_for_provider(&s, COPILOT_PROVIDER_ID);
    let switched = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "model", "value": copilot_value }),
        )
        .await
        .unwrap();
    assert_eq!(
        model_current_value(&switched),
        copilot_value,
        "selecting a copilot model makes it the current model: {switched}"
    );
    let ids = option_ids(&switched);
    assert!(
        ids.contains(&"mode".to_string()) && ids.contains(&"allow-all".to_string()),
        "activating copilot must surface its mode/allow-all selectors: {ids:?}"
    );

    // Prompt while copilot — the *second*-loaded, non-first provider — is
    // active. Its chain mints its own per-session id, but the host must remap
    // every outbound `notify-session` update to the group id (`sid`), or a
    // real editor would drop them as referring to an unknown session. Assert
    // both that copilot's text streams *and* that every streamed update
    // carries the group session id.
    let resp = host
        .request("session/prompt", prompt_text_params(&sid, "hi copilot"))
        .await
        .unwrap();
    assert!(resp.is_object(), "copilot prompt response: {resp}");
    let mut saw_copilot_text = false;
    let mut update_session_ids: Vec<String> = Vec::new();
    for _ in 0..50 {
        let Ok(msg) = host.recv_any().await else { break };
        if msg.get("method").and_then(|m| m.as_str()) != Some("session/update") {
            continue;
        }
        if let Some(s) = msg
            .get("params")
            .and_then(|p| p.get("sessionId"))
            .and_then(|s| s.as_str())
        {
            update_session_ids.push(s.to_string());
        }
        if serde_json::to_string(&msg)
            .unwrap_or_default()
            .contains("Copilot speaking")
        {
            saw_copilot_text = true;
            break;
        }
    }
    assert!(
        saw_copilot_text,
        "prompt should stream text from the active copilot provider"
    );
    assert!(
        !update_session_ids.is_empty() && update_session_ids.iter().all(|s| *s == sid),
        "updates from the switched (non-first) copilot provider must be remapped \
         to the group session id {sid}, got {update_session_ids:?}"
    );

    // Switch back to an ollama model → copilot's selectors disappear again.
    let ollama_value = value_for_provider(&switched, OLLAMA_PROVIDER_ID);
    let back = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "model", "value": ollama_value }),
        )
        .await
        .unwrap();
    assert_eq!(model_current_value(&back), ollama_value);
    let ids = option_ids(&back);
    assert!(
        !ids.contains(&"mode".to_string()) && !ids.contains(&"allow-all".to_string()),
        "returning to ollama must hide copilot's selectors: {ids:?}"
    );

    // A prompt now routes to the active (ollama) provider and streams text.
    let resp = host
        .request("session/prompt", prompt_text_params(&sid, "hi"))
        .await
        .unwrap();
    let mut saw_text = serde_json::to_string(&resp)
        .map(|s| s.contains("Hello") || s.contains("world"))
        .unwrap_or(false);
    while !saw_text {
        let Ok(msg) = host.recv_any().await else { break };
        let s = serde_json::to_string(&msg).unwrap_or_default();
        if s.contains("Hello") || s.contains("world") {
            saw_text = true;
        }
    }
    assert!(saw_text, "prompt should stream text from the active ollama provider");
}
