//! End-to-end test: the Copilot agent can make terminal (CLI) tool calls.
//!
//! The provider advertises a `run_terminal_command` tool to the model. When
//! the model calls it, the provider drives the host-owned `client.terminal`
//! resource: the host spawns the command (gated by the host-owned `terminal`
//! session config option), streams back the combined output plus exit status,
//! and the provider feeds that to the model on the next round.
//!
//! Both paths are exercised against a mocked Copilot chat API and a *real*
//! host process running a *real* command:
//!   * enabled  — the host runs `echo …` and the greeting round-trips back to
//!     the model,
//!   * disabled — the host refuses to spawn and the model is told terminal
//!     tools must be enabled.

mod common;

use common::*;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::time::{timeout, Duration};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL_ID: &str = "terminal-tool-model";
const CONTEXT_WINDOW: u64 = 128_000;
const MARKER: &str = "TERMINAL_HELLO_42";

/// Returns the i-th scripted response for the i-th matching request, so a
/// single mount can answer a multi-round tool-call conversation.
struct SequencedResponder {
    templates: Mutex<VecDeque<ResponseTemplate>>,
}

impl Respond for SequencedResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.templates.lock().unwrap().pop_front().unwrap_or_else(|| {
            ResponseTemplate::new(500).set_body_string("no more scripted responses")
        })
    }
}

/// A minimal mock of the Copilot API surface the provider touches, plus a
/// scripted two-round `/chat/completions` conversation.
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

    /// Script two streamed `/chat/completions` responses: the first asks the
    /// model to call `run_terminal_command` with `command`; the second is the
    /// final assistant text (issued after the tool result is fed back).
    async fn expect_chat_tool_then_text(&self, command: &str, final_text: &str) {
        // OpenAI wire shape: `arguments` is a JSON-encoded *string*.
        let arguments = json!({ "command": command }).to_string();
        let sse_tool = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{
                "index":0,
                "id":"call_term_1",
                "type":"function",
                "function":{"name":"run_terminal_command","arguments":arguments}
            }]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        );
        let sse_text = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":final_text},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        );
        let mk = |body: String| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream")
        };
        let responder = SequencedResponder {
            templates: Mutex::new(VecDeque::from(vec![mk(sse_tool), mk(sse_text)])),
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(responder)
            .mount(&self.server)
            .await;
    }

    /// Bodies of every `POST /chat/completions` the provider issued, in order.
    async fn chat_request_bodies(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/chat/completions")
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }
}

/// Content of the `role: "tool"` result message the provider appended to the
/// second chat round (i.e. what the model actually saw as the tool's output).
fn tool_result_content(body: &Value) -> String {
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        })
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
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

/// Drain the buffered `session/update` notifications emitted during the just
/// completed prompt turn and return their concatenated JSON. Uses a short
/// timeout because the turn's updates are already queued once the prompt
/// response has been received.
async fn drain_updates(host: &mut HostProcess) -> String {
    let mut all = String::new();
    while let Ok(Ok(msg)) = timeout(Duration::from_millis(500), host.recv_any()).await {
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            all.push_str(&serde_json::to_string(&msg).unwrap_or_default());
            all.push('\n');
        }
    }
    all
}

/// Terminal enabled + autopilot (auto-approve): the model calls
/// `run_terminal_command`, the host really runs it, and the greeting flows
/// back both to the client (as a completed tool call) and to the model (on
/// the second chat round).
#[tokio::test]
async fn copilot_runs_terminal_command_when_enabled() {
    ensure_artifacts();
    assert!(copilot_provider_wasm().exists(), "run `just build` first");

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;
    copilot
        .expect_chat_tool_then_text(
            &format!("echo {MARKER}"),
            "Done — the terminal printed the greeting.",
        )
        .await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut host = spawn_host(&copilot, state_home.path()).await;

    host.request("initialize", initialize_params_with_boolean_config())
        .await
        .unwrap();
    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    // Enable the host-owned terminal toggle, and auto-approve tool calls via
    // autopilot so no permission round-trip is needed.
    host.request(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "terminal", "type": "boolean", "value": true }),
    )
    .await
    .unwrap();
    host.request(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "mode", "value": "autopilot" }),
    )
    .await
    .unwrap();

    let resp = host
        .request("session/prompt", prompt_text_params(&sid, "run the echo command"))
        .await
        .unwrap();
    assert!(resp.is_object(), "prompt response: {resp}");

    // The provider ran the command and fed its real output back to the model
    // on the second chat round — the definitive proof the tool call executed.
    let bodies = copilot.chat_request_bodies().await;
    assert_eq!(
        bodies.len(),
        2,
        "expected two chat rounds (tool call, then final): {bodies:?}"
    );
    let tool_out = tool_result_content(&bodies[1]);
    assert!(
        tool_out.contains(MARKER),
        "the tool result fed to the model must carry the terminal output: {tool_out}"
    );

    // The client also observed the terminal tool call complete with output.
    let updates = drain_updates(&mut host).await;
    assert!(
        updates.contains(MARKER),
        "expected a tool-call update carrying the terminal output, got: {updates}"
    );
}

/// Terminal left at its safe default (off): the model still calls
/// `run_terminal_command`, but the host refuses to spawn and the provider
/// tells the model terminal tools must be enabled — without erroring the turn.
#[tokio::test]
async fn copilot_terminal_refused_when_disabled() {
    ensure_artifacts();
    assert!(copilot_provider_wasm().exists(), "run `just build` first");

    let copilot = CopilotMock::start().await;
    copilot.expect_token_exchange_404().await;
    copilot.expect_models().await;
    copilot
        .expect_chat_tool_then_text(&format!("echo {MARKER}"), "Understood — I'll stop.")
        .await;

    let state_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut host = spawn_host(&copilot, state_home.path()).await;

    // No boolean-config opt-in and no terminal toggle: terminal stays off.
    host.request("initialize", initialize_params())
        .await
        .unwrap();
    let s = host
        .request("session/new", new_session_params(cwd.path()))
        .await
        .unwrap();
    let sid = s.get("sessionId").and_then(Value::as_str).unwrap().to_string();

    // Auto-approve so we reach the dispatch path (and the host's refusal).
    host.request(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "mode", "value": "autopilot" }),
    )
    .await
    .unwrap();

    let resp = host
        .request("session/prompt", prompt_text_params(&sid, "run the echo command"))
        .await
        .unwrap();
    assert!(
        resp.get("error").is_none(),
        "a disabled terminal must not error the whole turn: {resp}"
    );

    // The model was told, on the second round, that terminal tools are off —
    // rather than receiving the command output.
    let bodies = copilot.chat_request_bodies().await;
    assert_eq!(bodies.len(), 2, "expected two chat rounds: {bodies:?}");
    let tool_out = tool_result_content(&bodies[1]);
    assert!(
        tool_out.contains("disabled"),
        "the tool result must tell the model terminal tools are disabled: {tool_out}"
    );
    assert!(
        !tool_out.contains(MARKER),
        "the command must NOT have produced output while terminal was disabled: {tool_out}"
    );
}
