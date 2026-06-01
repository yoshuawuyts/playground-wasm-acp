//! End-to-end test for filesystem mounts.
//!
//! Declares a host-directory mount in a global config file (pointed at by
//! `XDG_CONFIG_HOME`), redirects the provider's session storage to that
//! mount via `ACP_DATA_ROOT`, runs a prompt turn, and asserts the agent
//! actually read/wrote the mounted directory — i.e. a location entirely
//! outside the session cwd, served through the host's `wasi:filesystem`
//! preopen for `/<name>`.

mod common;

use std::time::Duration;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn agent_reads_writes_a_configured_mount() {
    ensure_artifacts();

    // The host-side directory that backs the `/scratch` mount. The agent
    // never sees this path; it only ever touches `/scratch`.
    let mount_dir = tempfile::tempdir().unwrap();

    // A global host config declaring `[mounts.scratch] path = <mount_dir>`,
    // discovered via XDG_CONFIG_HOME -> acp-wasm/config.toml.
    let config_home = tempfile::tempdir().unwrap();
    let cfg_dir = config_home.path().join("acp-wasm");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            "[mounts.scratch]\npath = \"{}\"\n",
            mount_dir.path().display()
        ),
    )
    .unwrap();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;
    ollama.expect_show_with_tools().await;
    ollama
        .expect_chat(&[chat_text_chunk("stored!"), chat_done_chunk()])
        .await;

    let cwd = tempfile::tempdir().unwrap();

    let mut host = HostBuilder::new()
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .env("XDG_CONFIG_HOME", config_home.path().to_string_lossy())
        // Redirect the provider's session persistence from /data onto the
        // mount, so a successful prompt turn writes into the mounted dir.
        .env("ACP_DATA_ROOT", "/scratch")
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

    // The provider persists `/<root>/sessions/<id>.json` at the end of the
    // turn. With ACP_DATA_ROOT=/scratch that resolves, through the mount,
    // to `<mount_dir>/sessions/<id>.json`. Poll briefly for the write.
    let expected = mount_dir
        .path()
        .join("sessions")
        .join(format!("{session_id}.json"));
    let mut found = false;
    for _ in 0..40 {
        if expected.exists() {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "agent did not write through the mount: missing {}\nhost stderr:\n{}",
        expected.display(),
        host.stderr_snapshot().await
    );

    // The file is real, host-visible content written by the wasm guest via
    // the mounted `wasi:filesystem` preopen — confirm it round-trips.
    let bytes = std::fs::read(&expected).unwrap();
    assert!(!bytes.is_empty(), "persisted session file is empty");
    let parsed: Value = serde_json::from_slice(&bytes).expect("session json parses");
    assert!(
        parsed.get("history").is_some(),
        "expected session history in {}: {parsed}",
        expected.display()
    );
}
