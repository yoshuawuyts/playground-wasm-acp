//! End-to-end test: the Copilot provider advertises a **Mode** selector
//! (agent / plan / autopilot) and an **Allow All** auto-tool-approval toggle
//! (on / off) in its session config options — mirroring the selectors the
//! GitHub Copilot CLI exposes over ACP — and round-trips them through
//! `session/set_config_option`, including autopilot implying allow-all.

mod common;

use common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL_ID: &str = "mode-test-model";
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
    async fn expect_models(&self) {
        let body = json!({
            "data": [{
                "id": MODEL_ID,
                "name": MODEL_ID,
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
}

fn find_option<'a>(resp: &'a Value, id: &str) -> &'a Value {
    resp.get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| opts.iter().find(|o| o.get("id").and_then(Value::as_str) == Some(id)))
        .unwrap_or_else(|| panic!("config option `{id}` not found in: {resp}"))
}

fn current_value(resp: &Value, id: &str) -> String {
    find_option(resp, id)
        .get("currentValue")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("option `{id}` has no currentValue"))
        .to_string()
}

fn category(resp: &Value, id: &str) -> String {
    find_option(resp, id)
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("option `{id}` has no category"))
        .to_string()
}

fn option_values(resp: &Value, id: &str) -> Vec<String> {
    find_option(resp, id)
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("value").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn advertises_mode_and_allow_all_options() {
    ensure_artifacts();
    assert!(copilot_provider_wasm().exists(), "run `just build` first");

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .provider(copilot_provider_wasm())
        .with_keyring_store("mock")
        .env("COPILOT_GITHUB_TOKEN", "gho_e2e_faketoken")
        .env("COPILOT_BASE_URL", copilot.base_url())
        .env("COPILOT_TOKEN_URL", copilot.token_url())
        .env("COPILOT_MODEL", MODEL_ID)
        .env("XDG_STATE_HOME", state_home.path().to_str().unwrap())
        .spawn()
        .await
        .unwrap();

    host.request("initialize", initialize_params())
        .await
        .unwrap();

    // session/new advertises Mode + Allow All from the start.
    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    assert_eq!(category(&s, "mode"), "mode");
    assert_eq!(current_value(&s, "mode"), "agent", "default mode is agent: {s}");
    assert_eq!(
        option_values(&s, "mode"),
        vec!["agent", "plan", "autopilot"],
        "mode offers agent/plan/autopilot: {s}"
    );

    assert_eq!(category(&s, "allow-all"), "permissions");
    assert_eq!(
        current_value(&s, "allow-all"),
        "off",
        "allow-all defaults to off (safe): {s}"
    );
    assert_eq!(option_values(&s, "allow-all"), vec!["on", "off"]);

    // Switch to plan mode.
    let planned = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "mode", "value": "plan" }),
        )
        .await
        .unwrap();
    assert_eq!(current_value(&planned, "mode"), "plan");
    assert_eq!(
        current_value(&planned, "allow-all"),
        "off",
        "plan mode does not auto-approve: {planned}"
    );

    // Toggle allow-all on explicitly (still in plan mode).
    let allowed = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "allow-all", "value": "on" }),
        )
        .await
        .unwrap();
    assert_eq!(current_value(&allowed, "allow-all"), "on");

    // Switch to autopilot: it implies allow-all even if toggled off.
    host.request(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "allow-all", "value": "off" }),
    )
    .await
    .unwrap();
    let auto = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "mode", "value": "autopilot" }),
        )
        .await
        .unwrap();
    assert_eq!(current_value(&auto, "mode"), "autopilot");
    assert_eq!(
        current_value(&auto, "allow-all"),
        "on",
        "autopilot implies allow-all: {auto}"
    );

    // Unknown values are rejected.
    let bad = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "mode", "value": "bogus" }),
        )
        .await;
    assert!(bad.is_err(), "unknown mode must be rejected");
}
