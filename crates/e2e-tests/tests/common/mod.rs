//! Shared test harness: artifact discovery, host child-process driver,
//! ACP JSON-RPC client over stdio, and an Ollama HTTP mock built on
//! `wiremock`.
//!
//! Each test owns its own `OllamaMock` (random port) and `HostProcess`
//! (own tempdir for `XDG_STATE_HOME`) so tests can be parallelised once
//! they're stable. Drop on `HostProcess` kills the child and prints the
//! captured stderr — surfacing host panics in test output.

#![allow(dead_code)] // helpers used selectively per test

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const RECV_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Artifact discovery
// ---------------------------------------------------------------------------

/// Workspace root, computed from the `CARGO_MANIFEST_DIR` of this crate.
pub fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../playground-wasm-acp/crates/e2e-tests
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.parent() // crates/
        .and_then(|p| p.parent()) // workspace root
        .expect("workspace layout")
        .to_path_buf()
}

/// Path to the host binary built by `cargo build -p host`.
pub fn host_bin() -> PathBuf {
    workspace_root().join("target").join("debug").join("host")
}

/// Path to the ollama-provider wasm component.
pub fn provider_wasm() -> PathBuf {
    workspace_root()
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join("ollama_provider.wasm")
}

/// Path to the uppercase-layer wasm component.
pub fn layer_wasm() -> PathBuf {
    workspace_root()
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join("uppercase_layer.wasm")
}

/// Assert all build artifacts exist; otherwise instruct the user to run
/// `just build`. Call this at the top of every test.
pub fn ensure_artifacts() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();

    for p in [host_bin(), provider_wasm(), layer_wasm()] {
        assert!(
            p.exists(),
            "missing artifact: {}\nrun `just build` first.",
            p.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Ollama HTTP mock
// ---------------------------------------------------------------------------

/// Build a body of NDJSON (newline-delimited JSON) chunks suitable for a
/// streaming `/api/chat` response.
pub fn ndjson(values: &[Value]) -> String {
    let mut s = String::new();
    for v in values {
        s.push_str(&serde_json::to_string(v).unwrap());
        s.push('\n');
    }
    s
}

/// Build a `/api/chat` chunk that emits assistant content.
pub fn chat_text_chunk(content: &str) -> Value {
    json!({"message": {"role": "assistant", "content": content}, "done": false})
}

/// Build a `/api/chat` chunk that requests a tool call. `arguments` must
/// be a JSON object matching the tool's schema.
pub fn chat_tool_chunk(name: &str, arguments: Value) -> Value {
    json!({
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{"function": {"name": name, "arguments": arguments}}]
        },
        "done": false,
    })
}

/// Final chunk Ollama sends to terminate the stream.
pub fn chat_done_chunk() -> Value {
    json!({"message": {"role": "assistant", "content": ""}, "done": true})
}

/// Sequenced responder: returns the i-th `ResponseTemplate` from a fixed
/// list, advancing on every call. Used to script multi-turn `/api/chat`
/// stubs (e.g. tool call, then final answer).
struct SequencedResponder {
    templates: std::sync::Mutex<VecDeque<ResponseTemplate>>,
}

impl Respond for SequencedResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let mut g = self.templates.lock().unwrap();
        g.pop_front().unwrap_or_else(|| {
            ResponseTemplate::new(500).set_body_string("no more scripted responses")
        })
    }
}

pub struct OllamaMock {
    server: MockServer,
}

impl OllamaMock {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Self { server }
    }

    /// Full `/api/chat` URL to inject as `OLLAMA_URL` for the guest.
    pub fn chat_url(&self) -> String {
        format!("{}/api/chat", self.server.uri())
    }

    /// Expose the underlying [`MockServer`] so tests can mount custom
    /// `Mock`s (e.g. with `set_delay`).
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    /// Stub `GET /api/tags` to return the given model names.
    pub async fn expect_tags(&self, models: &[&str]) {
        let body = json!({
            "models": models.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
        });
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Stub `POST /api/show` to declare tool-calling support for any model.
    /// The provider probes `/api/show` to decide whether to send a `tools`
    /// array; without this stub it falls back to a non-tool-capable
    /// configuration.
    pub async fn expect_show_with_tools(&self) {
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"capabilities": ["tools"]})),
            )
            .mount(&self.server)
            .await;
    }

    /// Stub `POST /api/chat` to return a single streaming response made of
    /// the supplied NDJSON chunks. Use [`chat_text_chunk`] / [`chat_done_chunk`].
    pub async fn expect_chat(&self, chunks: &[Value]) {
        let body = ndjson(chunks);
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_raw(body, "application/x-ndjson"),
            )
            .mount(&self.server)
            .await;
    }

    /// Stub `POST /api/chat` with a sequence of streamed responses; the
    /// i-th call gets the i-th body. Useful for tool-call round-trips.
    pub async fn expect_chat_sequence(&self, sequence: &[Vec<Value>]) {
        let templates: VecDeque<_> = sequence
            .iter()
            .map(|chunks| {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_raw(ndjson(chunks), "application/x-ndjson")
            })
            .collect();
        let responder = SequencedResponder {
            templates: std::sync::Mutex::new(templates),
        };
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(responder)
            .mount(&self.server)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Host child process + JSON-RPC driver
// ---------------------------------------------------------------------------

pub struct HostProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Keep the tempdir alive for the lifetime of the process.
    _state_dir: TempDir,
    /// Stderr reader task handle, plus a buffer the task writes into.
    stderr_buf: std::sync::Arc<Mutex<String>>,
    /// Pending notifications/requests received from the host (out of order
    /// vs. responses). Drained by `recv_notification`.
    pending: VecDeque<Value>,
    next_id: AtomicI64,
}

pub struct HostBuilder {
    provider: PathBuf,
    layers: Vec<PathBuf>,
    env: Vec<(String, String)>,
    secrets: Option<PathBuf>,
}

impl HostBuilder {
    pub fn new() -> Self {
        Self {
            provider: provider_wasm(),
            layers: Vec::new(),
            env: Vec::new(),
            secrets: None,
        }
    }

    pub fn with_layer(mut self, p: PathBuf) -> Self {
        self.layers.push(p);
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// Pass `--secrets <path>` to the host.
    pub fn with_secrets(mut self, p: PathBuf) -> Self {
        self.secrets = Some(p);
        self
    }

    pub async fn spawn(self) -> Result<HostProcess> {
        let state_dir = tempfile::tempdir().context("tempdir for XDG_STATE_HOME")?;

        let mut cmd = Command::new(host_bin());
        cmd.arg("--provider").arg(&self.provider);
        for l in &self.layers {
            cmd.arg("--layer").arg(l);
        }
        if let Some(p) = &self.secrets {
            cmd.arg("--secrets").arg(p);
        }
        cmd.env("XDG_STATE_HOME", state_dir.path())
            .env("RUST_LOG", "host=debug,acp=debug");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("spawning host")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = child.stderr.take().unwrap();

        let stderr_buf = std::sync::Arc::new(Mutex::new(String::new()));
        spawn_stderr_drain(stderr, stderr_buf.clone());

        Ok(HostProcess {
            child,
            stdin,
            stdout,
            _state_dir: state_dir,
            stderr_buf,
            pending: VecDeque::new(),
            next_id: AtomicI64::new(1),
        })
    }
}

fn spawn_stderr_drain(stderr: ChildStderr, buf: std::sync::Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            // Best-effort write; buf is shared for post-mortem dump.
            if let Ok(mut g) = buf.try_lock() {
                g.push_str(&line);
                g.push('\n');
            }
            // Mirror to test stderr (visible only when --nocapture).
            eprintln!("[host] {line}");
        }
    });
}

impl HostProcess {
    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn write_msg(&mut self, msg: &Value) -> Result<()> {
        let mut s = serde_json::to_string(msg)?;
        s.push('\n');
        self.stdin.write_all(s.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Write a raw line (already JSON-encoded, with or without trailing
    /// newline) to the host's stdin. Used by tests that need to send a
    /// request with a chosen id and continue reading other traffic before
    /// the response arrives.
    pub async fn raw_write(&mut self, s: &str) -> Result<()> {
        self.stdin.write_all(s.as_bytes()).await?;
        if !s.ends_with('\n') {
            self.stdin.write_all(b"\n").await?;
        }
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read one line of JSON from stdout. Returns `Err` on EOF or timeout.
    async fn read_msg(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = timeout(RECV_TIMEOUT, self.stdout.read_line(&mut line))
            .await
            .map_err(|_| anyhow!("timeout waiting for host stdout"))??;
        if n == 0 {
            bail!("host stdout closed (EOF)");
        }
        let v: Value = serde_json::from_str(line.trim_end_matches('\n'))
            .with_context(|| format!("parse json: {line:?}"))?;
        Ok(v)
    }

    /// Send a JSON-RPC request and await the matching response.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_msg(&req).await?;
        loop {
            let msg = self.read_msg().await?;
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("rpc error for {method}: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            self.pending.push_back(msg);
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let n = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_msg(&n).await
    }

    /// Receive the next pending message (notification or inbound request
    /// from the host). Drains the in-memory queue first, then reads from
    /// stdout.
    pub async fn recv_any(&mut self) -> Result<Value> {
        if let Some(v) = self.pending.pop_front() {
            return Ok(v);
        }
        self.read_msg().await
    }

    /// Wait until a notification with the given method is observed, then
    /// return its `params`. Other messages are stashed back into the queue
    /// (responses) or dropped (other notifications) — callers that care
    /// about ordering should use [`recv_any`] directly.
    pub async fn wait_notification(&mut self, target_method: &str) -> Result<Value> {
        loop {
            let msg = self.recv_any().await?;
            if msg.get("id").is_none() {
                if msg.get("method").and_then(|m| m.as_str()) == Some(target_method) {
                    return Ok(msg.get("params").cloned().unwrap_or(Value::Null));
                }
            } else if msg.get("method").is_some() {
                // Inbound request from host (e.g. fs/read_text_file): keep for caller.
                self.pending.push_front(msg);
                bail!("expected notification {target_method}, got inbound request first");
            } else {
                // Stray response, requeue.
                self.pending.push_back(msg);
            }
        }
    }

    /// Respond to an inbound JSON-RPC request from the host (e.g.
    /// `fs/read_text_file`).
    pub async fn respond(&mut self, id: Value, result: Value) -> Result<()> {
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.write_msg(&resp).await
    }

    /// Convenience: wait for the next inbound request whose method equals
    /// `target_method`, returning `(id, params)`. Other messages are
    /// queued for later inspection.
    pub async fn wait_inbound_request(&mut self, target_method: &str) -> Result<(Value, Value)> {
        let mut requeue: VecDeque<Value> = VecDeque::new();
        let result = loop {
            let msg = self.recv_any().await?;
            let is_req = msg.get("id").is_some() && msg.get("method").is_some();
            if is_req && msg.get("method").and_then(|m| m.as_str()) == Some(target_method) {
                let id = msg.get("id").cloned().unwrap();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                break Ok((id, params));
            }
            requeue.push_back(msg);
        };
        for m in requeue.into_iter().rev() {
            self.pending.push_front(m);
        }
        result
    }

    /// Captured host stderr (best-effort snapshot).
    pub async fn stderr_snapshot(&self) -> String {
        self.stderr_buf.lock().await.clone()
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        // best-effort kill
        let _ = self.child.start_kill();
        if let Ok(g) = self.stderr_buf.try_lock()
            && !g.is_empty()
            && std::thread::panicking()
        {
            eprintln!("\n--- host stderr ---\n{}\n--- end ---", *g);
        }
    }
}

// ---------------------------------------------------------------------------
// ACP message builders (well-known method names + minimal params)
// ---------------------------------------------------------------------------

/// Build params for `initialize`. Capabilities are minimal: fs read/write
/// enabled, terminal disabled.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": true, "writeTextFile": true},
            "terminal": false
        }
    })
}

/// Build params for `session/new` rooted at `cwd`.
pub fn new_session_params(cwd: &Path) -> Value {
    json!({
        "cwd": cwd.to_string_lossy(),
        "mcpServers": []
    })
}

/// Build params for `session/prompt` with a single user text block.
pub fn prompt_text_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": text}]
    })
}
