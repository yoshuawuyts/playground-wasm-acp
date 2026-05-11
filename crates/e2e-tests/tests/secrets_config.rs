//! End-to-end tests for the host's `--secrets <path>` flag.
//!
//! These cover only the host-side wiring: registry parsing, CLI plumbing,
//! and that loading a valid config doesn't interfere with the normal ACP
//! handshake. The guest components don't (yet) call
//! `wasmcloud:secrets/store.get`, so we don't assert on guest-visible
//! behaviour.

mod common;

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use common::*;
use tokio::process::Command;
use tokio::time::timeout;

/// Valid `--secrets` config: host should start normally and complete the
/// `initialize` handshake.
#[tokio::test]
async fn valid_secrets_config_initializes() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;

    let mut cfg = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
    cfg.write_all(
        br#"
[ollama_provider]
api_key = { value = "hunter2" }

[uppercase_layer]
shared = { value = "for-layer" }
"#,
    )
    .unwrap();

    let mut host = HostBuilder::new()
        .with_secrets(cfg.path().to_path_buf())
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    let resp = host.request("initialize", initialize_params()).await.unwrap();
    assert!(resp.is_object(), "expected object response, got: {resp}");
    assert!(
        resp.get("protocolVersion").is_some(),
        "missing protocolVersion in: {resp}"
    );

    // Confirm the host logged that it loaded the config (best-effort —
    // surfaces wiring regressions where the flag is silently ignored).
    let err = host.stderr_snapshot().await;
    assert!(
        err.contains("loaded secrets config"),
        "expected `loaded secrets config` in stderr, got:\n{err}"
    );
}

/// Invalid `--secrets` config: host should exit non-zero before reading
/// stdin, with a clear parse error on stderr.
#[tokio::test]
async fn invalid_secrets_config_exits_with_error() {
    ensure_artifacts();

    // Both `value` and `command` set — rejected at load time.
    let mut cfg = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
    cfg.write_all(
        br#"
[ollama_provider]
bad = { value = "x", command = ["echo", "y"] }
"#,
    )
    .unwrap();

    let state_dir = tempfile::tempdir().unwrap();
    let output = timeout(
        Duration::from_secs(15),
        Command::new(host_bin())
            .arg("--provider")
            .arg(provider_wasm())
            .arg("--secrets")
            .arg(cfg.path())
            .env("XDG_STATE_HOME", state_dir.path())
            .env("RUST_LOG", "host=debug")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("host did not exit within timeout")
    .expect("spawn host");

    assert!(
        !output.status.success(),
        "host should fail on invalid secrets config; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("loading secrets config") || stderr.contains("secrets["),
        "expected secrets-config error in stderr, got:\n{stderr}"
    );
}
