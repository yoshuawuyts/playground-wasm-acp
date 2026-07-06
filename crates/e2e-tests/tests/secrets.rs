//! End-to-end tests for the per-component keyring secret store.
//!
//! These cover host-side wiring only: that the keyring store is
//! initialized on startup, that a valid handshake still completes, and
//! that the `secret set` / `secret delete` admin subcommands run. The
//! sample guests don't call `wasmcloud:secrets/store.get`, and the `mock`
//! store is per-process (a separate `secret set` run can't seed the host
//! process), so guest-visible secret *values* are exercised by the host
//! crate's unit tests rather than here.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::*;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

/// With `--keyring-store mock`, the host initializes the keyring store on
/// startup and completes the `initialize` handshake normally.
#[tokio::test]
async fn keyring_store_initializes_on_startup() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;

    let mut host = HostBuilder::new()
        .with_keyring_store("mock")
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    let resp = host
        .request("initialize", initialize_params())
        .await
        .unwrap();
    assert!(
        resp.get("protocolVersion").is_some(),
        "missing protocolVersion in: {resp}"
    );

    let err = host.stderr_snapshot().await;
    assert!(
        err.contains("initialized keyring store"),
        "expected `initialized keyring store` in stderr, got:\n{err}"
    );
}

/// The `secret set` / `secret delete` admin subcommands run against the
/// selected store, read the value from stdin, and exit zero without
/// running the ACP host. (With `mock` nothing persists across processes;
/// this asserts the CLI plumbing + provisioning path.)
#[tokio::test]
async fn secret_set_and_delete_via_cli() {
    ensure_artifacts();

    // `secret set <component> <key>` — value on stdin.
    let mut child = Command::new(host_bin())
        .arg("--keyring-store")
        .arg("mock")
        .arg("secret")
        .arg("set")
        .arg("ollama_provider")
        .arg("api_key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `secret set`");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hunter2\n")
        .await
        .unwrap();
    let set = timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .expect("`secret set` timed out")
        .expect("`secret set` output");
    assert!(
        set.status.success(),
        "`secret set` failed: {}",
        String::from_utf8_lossy(&set.stderr)
    );
    let set_err = String::from_utf8_lossy(&set.stderr);
    assert!(
        set_err.contains("stored secret"),
        "expected `stored secret` in stderr, got:\n{set_err}"
    );

    // `secret delete <component> <key>` — idempotent, no stdin.
    let del = timeout(
        Duration::from_secs(15),
        Command::new(host_bin())
            .arg("--keyring-store")
            .arg("mock")
            .arg("secret")
            .arg("delete")
            .arg("ollama_provider")
            .arg("api_key")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("`secret delete` timed out")
    .expect("`secret delete` output");
    assert!(
        del.status.success(),
        "`secret delete` failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
    let del_err = String::from_utf8_lossy(&del.stderr);
    assert!(
        del_err.contains("deleted secret"),
        "expected `deleted secret` in stderr, got:\n{del_err}"
    );
}
