//! Smoke test: ACP `initialize` handshake. Verifies the host launches,
//! loads the wasm chain, and returns a well-formed initialize response.
//! No `/api/chat` traffic is expected; `/api/tags` is stubbed defensively.

mod common;

use common::*;

#[tokio::test]
async fn initialize_handshake() {
    ensure_artifacts();

    let ollama = OllamaMock::start().await;
    ollama.expect_tags(&["llama3.2"]).await;

    let mut host = HostBuilder::new()
        .env("OLLAMA_URL", ollama.chat_url())
        .env("OLLAMA_MODEL", "llama3.2")
        .spawn()
        .await
        .unwrap();

    let resp = host.request("initialize", initialize_params()).await.unwrap();
    // Response shape: a JSON object with at least a `protocolVersion`.
    assert!(
        resp.is_object(),
        "expected object response, got: {resp}"
    );
    assert!(
        resp.get("protocolVersion").is_some(),
        "missing protocolVersion in: {resp}"
    );
}
