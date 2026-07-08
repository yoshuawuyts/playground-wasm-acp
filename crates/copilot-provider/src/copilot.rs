//! Minimal client for the GitHub Copilot chat API.
//!
//! Authentication has two possible shapes:
//!
//! 1. **Token exchange (editor apps).** A GitHub token minted by a
//!    Copilot-enabled *editor* OAuth app is exchanged at
//!    `GET https://api.github.com/copilot_internal/v2/token` for a
//!    short-lived Copilot API token plus the account's API base URL. This
//!    endpoint returns `404` for tokens it doesn't recognise — notably a
//!    `gh auth token` from the GitHub CLI, or a fine-grained PAT — even
//!    though those same tokens work fine against the chat API.
//! 2. **Direct token (fallback).** When the exchange is unavailable we send
//!    the raw GitHub token (OAuth `gho_`, GitHub App `ghu_`, or a fine-grained
//!    PAT `github_pat_` with the *Copilot Requests* permission) straight to
//!    the chat API as a bearer token.
//!
//! Either way the resulting token authenticates
//! `POST {base}/chat/completions` (OpenAI-compatible, streamed as
//! Server-Sent Events) and `GET {base}/models`, and is cached in-process.
//!
//! The raw GitHub token is resolved from the host secrets store (key
//! `github_token`, scoped to this component id) with an environment
//! fallback. See [`resolve_github_token`].

use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use wstd::http::{Body, BodyExt, Client, Method, Request};

use acp_wasm_sys::provider::wasmcloud::secrets::reveal;
use acp_wasm_sys::provider::wasmcloud::secrets::store::{self, SecretValue};

const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE_URL: &str = "https://api.githubcopilot.com";
const DEFAULT_MODEL: &str = "gpt-4o";
const DEFAULT_EDITOR_VERSION: &str = "vscode/1.104.1";
const DEFAULT_INTEGRATION_ID: &str = "vscode-chat";
const USER_AGENT: &str = "playground-wasm-acp/0.1";

/// Refresh the Copilot token this many seconds before it actually expires.
const REFRESH_MARGIN_SECS: u64 = 120;

/// How long a direct (un-exchanged) GitHub token is cached before the raw
/// token is re-resolved. A long-lived GitHub token carries no server-provided
/// expiry, so this is just a periodic refresh; a mid-session `401` forces an
/// earlier re-resolution via [`invalidate_token`].
const DIRECT_TOKEN_TTL_SECS: u64 = 8 * 3600;

/// Environment variables checked (in order) for a raw GitHub token when
/// the secrets store has none. Matches the Copilot CLI's precedence.
const TOKEN_ENV_VARS: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

/// The default model id. Overridable via `COPILOT_MODEL`.
pub fn default_model() -> String {
    std::env::var("COPILOT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
}

/// The `Editor-Version` header value. Overridable via `COPILOT_EDITOR_VERSION`.
fn editor_version() -> String {
    std::env::var("COPILOT_EDITOR_VERSION").unwrap_or_else(|_| DEFAULT_EDITOR_VERSION.to_string())
}

/// The `Copilot-Integration-Id` header value. Overridable via
/// `COPILOT_INTEGRATION_ID`.
fn integration_id() -> String {
    std::env::var("COPILOT_INTEGRATION_ID").unwrap_or_else(|_| DEFAULT_INTEGRATION_ID.to_string())
}

/// Explicit API base URL override (`COPILOT_BASE_URL`). When unset, the base
/// URL is taken from the token-exchange response (`endpoints.api`), then a
/// `proxy-ep` fallback, then [`DEFAULT_BASE_URL`].
fn base_url_override() -> Option<String> {
    std::env::var("COPILOT_BASE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// The token-exchange endpoint. Overridable via `COPILOT_TOKEN_URL` (chiefly
/// so tests can point the exchange at a local mock); defaults to the real
/// GitHub endpoint [`TOKEN_EXCHANGE_URL`].
fn token_exchange_url() -> String {
    std::env::var("COPILOT_TOKEN_URL").unwrap_or_else(|_| TOKEN_EXCHANGE_URL.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -----------------------------------------------------------------------------
// Raw GitHub token resolution (secrets → env)
// -----------------------------------------------------------------------------

/// Resolve a raw GitHub token usable for the Copilot token exchange.
///
/// Precedence: the host secrets store (key `github_token`) first, then the
/// [`TOKEN_ENV_VARS`] environment variables. Classic PATs (`ghp_`) are
/// rejected with a clear message because the Copilot API doesn't accept them.
pub fn resolve_github_token() -> Result<String, String> {
    if let Ok(secret) = store::get("github_token") {
        if let SecretValue::String(s) = reveal::reveal(&secret) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                validate_token_prefix(&s)?;
                return Ok(s);
            }
        }
    }

    for var in TOKEN_ENV_VARS {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                validate_token_prefix(&v)?;
                return Ok(v);
            }
        }
    }

    Err("no GitHub token configured: store one in the copilot provider's secret \
         store (`cargo run -p host -- secret set local:copilot_provider github_token`) \
         or set COPILOT_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN"
        .to_string())
}

/// Reject token types the Copilot API cannot use. Classic PATs (`ghp_`) are
/// unsupported; OAuth (`gho_`), GitHub App (`ghu_`), and fine-grained PAT
/// (`github_pat_`) tokens are accepted.
fn validate_token_prefix(token: &str) -> Result<(), String> {
    if token.starts_with("ghp_") {
        return Err("classic personal access tokens (ghp_…) are not supported by the \
                    Copilot API; use an OAuth token (gho_…), a GitHub App token \
                    (ghu_…), or a fine-grained PAT (github_pat_…) with the \
                    \"Copilot Requests\" permission"
            .to_string());
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Copilot token exchange + cache
// -----------------------------------------------------------------------------

/// A resolved Copilot API token plus the account-specific base URL it must be
/// used against, and the epoch second it expires.
#[derive(Clone)]
pub struct CopilotToken {
    pub token: String,
    pub base_url: String,
    pub expires_at: u64,
}

// Single-threaded wasm guest: a thread-local cache is enough.
thread_local! {
    static TOKEN_CACHE: RefCell<Option<CopilotToken>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
struct TokenExchangeResponse {
    token: String,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    endpoints: Option<Endpoints>,
}

#[derive(Deserialize)]
struct Endpoints {
    #[serde(default)]
    api: Option<String>,
}

/// Return a valid Copilot token, exchanging (and caching) a fresh one if the
/// cache is empty or close to expiry.
pub async fn copilot_token() -> Result<CopilotToken, String> {
    if let Some(cached) = TOKEN_CACHE.with(|c| c.borrow().clone()) {
        if now_secs() + REFRESH_MARGIN_SECS < cached.expires_at {
            return Ok(cached);
        }
    }
    let github_token = resolve_github_token()?;
    let fresh = match try_exchange(&github_token).await? {
        Some(exchanged) => exchanged,
        // The exchange endpoint doesn't accept this token (gh-CLI tokens and
        // fine-grained PATs 404 there); use it directly against the chat API.
        None => direct_token(github_token),
    };
    TOKEN_CACHE.with(|c| *c.borrow_mut() = Some(fresh.clone()));
    Ok(fresh)
}

/// Force the cached token to be discarded so the next [`copilot_token`] call
/// re-exchanges. Used to recover from a mid-session `401`.
pub fn invalidate_token() {
    TOKEN_CACHE.with(|c| *c.borrow_mut() = None);
}

/// Try to exchange a raw GitHub token for a short-lived Copilot API token.
///
/// Returns `Ok(Some(_))` on success. Returns `Ok(None)` when the exchange
/// endpoint rejects the token with any non-success status — expected for
/// GitHub CLI tokens and fine-grained PATs, which the exchange endpoint `404`s
/// but the chat API accepts directly, so the caller falls back to
/// [`direct_token`]. Only a transport failure yields `Err`.
async fn try_exchange(github_token: &str) -> Result<Option<CopilotToken>, String> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(token_exchange_url())
        .header("authorization", format!("token {github_token}"))
        .header("editor-version", editor_version())
        .header("user-agent", USER_AGENT)
        .header("accept", "application/json")
        .body(Body::empty())
        .map_err(|e| format!("build token request: {e}"))?;

    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| format!("token exchange send: {e}"))?;
    if !resp.status().is_success() {
        // Exchange unavailable for this token (commonly `404` for gh-CLI
        // tokens and fine-grained PATs). Signal the caller to use the token
        // directly rather than surfacing a misleading "Not Found".
        return Ok(None);
    }

    let body = resp
        .body_mut()
        .json::<TokenExchangeResponse>()
        .await
        .map_err(|e| format!("decode token exchange: {e}"))?;

    let base_url = base_url_override()
        .or_else(|| {
            body.endpoints
                .as_ref()
                .and_then(|e| e.api.as_deref())
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| derive_base_url_from_proxy_ep(&body.token))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let expires_at = body.expires_at.unwrap_or_else(|| now_secs() + 1800);

    Ok(Some(CopilotToken {
        token: body.token,
        base_url,
        expires_at,
    }))
}

/// Build a direct-auth token: the raw GitHub token is used as the chat API
/// bearer token, against the `COPILOT_BASE_URL` override or [`DEFAULT_BASE_URL`].
/// Used when the token exchange is unavailable (see [`try_exchange`]).
fn direct_token(github_token: String) -> CopilotToken {
    CopilotToken {
        token: github_token,
        base_url: base_url_override().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        expires_at: now_secs() + DIRECT_TOKEN_TTL_SECS,
    }
}

/// Derive an API base URL from the `proxy-ep` field embedded in an exchanged
/// Copilot token (a `key=value;…` string). Enterprise/proxied accounts carry
/// `proxy-ep=proxy.<host>`; the API host replaces the leading `proxy.` with
/// `api.`. Returns `None` when absent (individual accounts).
fn derive_base_url_from_proxy_ep(token: &str) -> Option<String> {
    for part in token.split(';') {
        let part = part.trim();
        if let Some(ep) = part.strip_prefix("proxy-ep=") {
            let ep = ep
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            if ep.is_empty() {
                return None;
            }
            let host = match ep.strip_prefix("proxy.") {
                Some(rest) => format!("api.{rest}"),
                None => ep.to_string(),
            };
            return Some(format!("https://{host}"));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Chat messages
// -----------------------------------------------------------------------------

/// One message in an OpenAI-style chat history. Owned so it can be kept across
/// prompt turns and persisted to `/data`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

// -----------------------------------------------------------------------------
// Chat completions (streaming SSE)
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

/// The meaning of a single SSE line from the chat endpoint.
#[derive(Debug, PartialEq)]
enum SseEvent {
    /// A text fragment to append to (and stream out from) the reply.
    Content(String),
    /// The `data: [DONE]` sentinel that terminates the stream.
    Done,
    /// A line to skip (blank line, comment, non-content delta, unparseable).
    Ignore,
}

/// Parse one line of the SSE stream. Pure and total so it stays easy to reason
/// about; the streaming loop feeds it newline-delimited lines.
fn parse_sse_line(line: &str) -> SseEvent {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(data) = line.strip_prefix("data:") else {
        // Blank lines, `event:`/`id:` fields, and `:` comments are ignored.
        return SseEvent::Ignore;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseEvent::Done;
    }
    match serde_json::from_str::<StreamChunk>(data) {
        Ok(chunk) => {
            let text: String = chunk
                .choices
                .into_iter()
                .filter_map(|c| c.delta.content)
                .collect();
            if text.is_empty() {
                SseEvent::Ignore
            } else {
                SseEvent::Content(text)
            }
        }
        Err(_) => SseEvent::Ignore,
    }
}

/// Send a streaming chat completion. `on_chunk` is invoked once per non-empty
/// content fragment as it arrives; the full assembled reply is returned.
///
/// A `401`/`403` triggers a single forced token refresh and retry, to survive
/// a Copilot token that expired mid-session.
pub async fn chat<F, Fut>(
    model: String,
    history: Vec<Message>,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(String) -> Fut,
    Fut: core::future::Future<Output = ()>,
{
    match chat_once(&model, &history, &mut on_chunk).await {
        Err(ChatError::Auth(_)) => {
            invalidate_token();
            chat_once(&model, &history, &mut on_chunk)
                .await
                .map_err(|e| e.into_string())
        }
        other => other.map_err(|e| e.into_string()),
    }
}

enum ChatError {
    /// `401`/`403` — token likely expired; caller may refresh and retry.
    Auth(String),
    /// Any other failure.
    Other(String),
}

impl ChatError {
    fn into_string(self) -> String {
        match self {
            ChatError::Auth(s) | ChatError::Other(s) => s,
        }
    }
}

async fn chat_once<F, Fut>(
    model: &str,
    history: &[Message],
    on_chunk: &mut F,
) -> Result<String, ChatError>
where
    F: FnMut(String) -> Fut,
    Fut: core::future::Future<Output = ()>,
{
    let tok = copilot_token().await.map_err(ChatError::Other)?;
    let url = format!("{}/chat/completions", tok.base_url);
    let body = Body::from_json(&ChatRequest {
        model,
        messages: history,
        stream: true,
    })
    .map_err(|e| ChatError::Other(format!("encode chat request: {e}")))?;

    let req = Request::builder()
        .method(Method::POST)
        .uri(&url)
        .header("authorization", format!("Bearer {}", tok.token))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("copilot-integration-id", integration_id())
        .header("editor-version", editor_version())
        .header("user-agent", USER_AGENT)
        .header("openai-intent", "conversation-panel")
        .header("x-initiator", "user")
        .body(body)
        .map_err(|e| ChatError::Other(format!("build chat request: {e}")))?;

    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| ChatError::Other(format!("chat send: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .body_mut()
            .str_contents()
            .await
            .unwrap_or("<unreadable>")
            .to_string();
        let msg = format!("copilot chat HTTP {status}: {txt}");
        return Err(if status.as_u16() == 401 || status.as_u16() == 403 {
            ChatError::Auth(msg)
        } else {
            ChatError::Other(msg)
        });
    }

    let mut stream = resp.into_body().into_boxed_body().into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    'outer: while let Some(frame) = stream.next().await {
        let bytes = frame.map_err(|e| ChatError::Other(format!("read chat body: {e}")))?;
        buf.extend_from_slice(&bytes);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            match parse_sse_line(line) {
                SseEvent::Content(text) => {
                    on_chunk(text.clone()).await;
                    content.push_str(&text);
                }
                SseEvent::Done => break 'outer,
                SseEvent::Ignore => {}
            }
        }
    }
    // Handle a trailing line with no terminating newline (rare).
    if let Ok(line) = std::str::from_utf8(&buf) {
        if let SseEvent::Content(text) = parse_sse_line(line) {
            on_chunk(text.clone()).await;
            content.push_str(&text);
        }
    }

    Ok(content)
}

// -----------------------------------------------------------------------------
// Model listing
// -----------------------------------------------------------------------------

/// A chat-capable model advertised by the account.
#[derive(Clone)]
pub struct CopilotModel {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
}

#[derive(Deserialize)]
struct ModelCapabilities {
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// List chat-capable models via `GET {base}/models`, de-duplicated by id and
/// preserving the order the API returns them.
pub async fn list_models() -> Result<Vec<CopilotModel>, String> {
    let tok = copilot_token().await?;
    let url = format!("{}/models", tok.base_url);
    let req = Request::builder()
        .method(Method::GET)
        .uri(&url)
        .header("authorization", format!("Bearer {}", tok.token))
        .header("copilot-integration-id", integration_id())
        .header("editor-version", editor_version())
        .header("user-agent", USER_AGENT)
        .header("accept", "application/json")
        .body(Body::empty())
        .map_err(|e| format!("build models request: {e}"))?;

    let mut resp = Client::new()
        .send(req)
        .await
        .map_err(|e| format!("models send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp
            .body_mut()
            .str_contents()
            .await
            .unwrap_or("<unreadable>")
            .to_string();
        return Err(format!("copilot models HTTP {status}: {txt}"));
    }

    let body = resp
        .body_mut()
        .json::<ModelsResponse>()
        .await
        .map_err(|e| format!("decode models: {e}"))?;

    let mut out: Vec<CopilotModel> = Vec::new();
    for entry in body.data {
        // Skip non-chat models (e.g. embeddings) when the capability type is
        // advertised; keep entries that don't declare one.
        if let Some(caps) = &entry.capabilities {
            if let Some(kind) = &caps.kind {
                if kind != "chat" {
                    continue;
                }
            }
        }
        if out.iter().any(|m| m.id == entry.id) {
            continue;
        }
        let name = entry.name.unwrap_or_else(|| entry.id.clone());
        out.push(CopilotModel { id: entry.id, name });
    }
    Ok(out)
}
