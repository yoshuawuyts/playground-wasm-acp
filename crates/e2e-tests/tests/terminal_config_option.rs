//! End-to-end test: the host-owned `terminal` boolean config option.
//!
//! Terminal (CLI) execution is gated behind a host-owned boolean session
//! config option (id `terminal`, default `false`) instead of the client's
//! `initialize` terminal capability. Per the ACP boolean-config-option
//! RFD the host only advertises it to clients that opted into boolean
//! config options (`session.configOptions.boolean`), and toggling it
//! round-trips through `session/set_config_option` with a boolean value.

mod common;

use common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL_ID: &str = "terminal-test-model";
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

fn find_option<'a>(resp: &'a Value, id: &str) -> Option<&'a Value> {
    resp.get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| opts.iter().find(|o| o.get("id").and_then(Value::as_str) == Some(id)))
}

fn option_type(resp: &Value, id: &str) -> String {
    find_option(resp, id)
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("option `{id}` has no type in: {resp}"))
        .to_string()
}

fn bool_current_value(resp: &Value, id: &str) -> bool {
    find_option(resp, id)
        .and_then(|o| o.get("currentValue"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("option `{id}` has no boolean currentValue in: {resp}"))
}

async fn spawn_host(copilot: &CopilotMock, state_home: &std::path::Path) -> HostProcess {
    HostBuilder::new()
        .provider(copilot_provider_wasm())
        .with_keyring_store("mock")
        .env("COPILOT_GITHUB_TOKEN", "gho_e2e_faketoken")
        .env("COPILOT_BASE_URL", copilot.base_url())
        .env("COPILOT_TOKEN_URL", copilot.token_url())
        .env("COPILOT_MODEL", MODEL_ID)
        .env("XDG_STATE_HOME", state_home.to_str().unwrap())
        .spawn()
        .await
        .unwrap()
}

#[tokio::test]
async fn advertises_and_toggles_terminal_when_client_opts_in() {
    ensure_artifacts();
    assert!(copilot_provider_wasm().exists(), "run `just build` first");

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut host = spawn_host(&copilot, state_home.path()).await;

    // Client advertises boolean config option support.
    host.request("initialize", initialize_params_with_boolean_config())
        .await
        .unwrap();

    // session/new advertises the host-owned `terminal` toggle, default off.
    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    assert!(
        find_option(&s, "terminal").is_some(),
        "terminal option must be advertised when client opted in: {s}"
    );
    assert_eq!(option_type(&s, "terminal"), "boolean", "terminal is a boolean option: {s}");
    assert_eq!(
        bool_current_value(&s, "terminal"),
        false,
        "terminal defaults to off (safe): {s}"
    );

    // Toggle it on with a boolean value.
    let on = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "terminal", "type": "boolean", "value": true }),
        )
        .await
        .unwrap();
    assert_eq!(bool_current_value(&on, "terminal"), true, "terminal now on: {on}");

    // The guest's own selectors survive the host-owned toggle.
    assert!(
        find_option(&on, "mode").is_some(),
        "guest selectors remain after toggling terminal: {on}"
    );

    // Toggle it back off.
    let off = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "terminal", "type": "boolean", "value": false }),
        )
        .await
        .unwrap();
    assert_eq!(bool_current_value(&off, "terminal"), false, "terminal back off: {off}");

    // A non-boolean value for the boolean option is rejected.
    let bad = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "terminal", "value": "yes" }),
        )
        .await;
    assert!(bad.is_err(), "terminal requires a boolean value, string must be rejected");
}

#[tokio::test]
async fn omits_terminal_option_without_boolean_capability() {
    ensure_artifacts();
    assert!(copilot_provider_wasm().exists(), "run `just build` first");

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut host = spawn_host(&copilot, state_home.path()).await;

    // Client does NOT advertise boolean config option support.
    host.request("initialize", initialize_params())
        .await
        .unwrap();

    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    assert!(
        find_option(&s, "terminal").is_none(),
        "terminal option must be hidden from clients that didn't opt in: {s}"
    );
    // The guest's normal selectors are still advertised.
    assert!(
        find_option(&s, "mode").is_some(),
        "guest selectors are still advertised: {s}"
    );

    // The capability is not merely an advertisement filter: a client that
    // did not opt in cannot enable the hidden boolean option by guessing its
    // id and sending the typed setter directly.
    let hidden_write = host
        .request(
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "terminal", "type": "boolean", "value": true }),
        )
        .await;
    assert!(
        hidden_write.is_err(),
        "terminal must remain unavailable without boolean capability"
    );
}
