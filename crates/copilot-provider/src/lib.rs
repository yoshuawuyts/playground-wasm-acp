//! ACP wasm provider that forwards prompts to the GitHub Copilot chat API.
//!
//! Mirrors `ollama-provider` but targets Copilot: it resolves a GitHub token
//! from the host secrets store (or env), exchanges it for a short-lived
//! Copilot API token, and streams OpenAI-compatible chat completions back to
//! the editor as `session/update` notifications. Text only — no tool calls.

mod copilot;
mod storage;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use acp_wasm_sys::provider::exports::yosh::acp::agent::{
    Guest, GuestPromptTurn, GuestSession, PromptTurn, Session,
};
use acp_wasm_sys::provider::yosh::acp::content::{ContentBlock, TextContent};
use acp_wasm_sys::provider::yosh::acp::errors::{Error, ErrorCode};
use acp_wasm_sys::provider::yosh::acp::init::{
    AgentCapabilities, AuthenticateRequest, ImplementationInfo, InitializeRequest,
    InitializeResponse, McpCapabilities, PromptCapabilities, SessionCapabilities,
};
use acp_wasm_sys::provider::yosh::acp::prompts::{PromptResponse, SessionUpdate, StopReason};
use acp_wasm_sys::provider::yosh::acp::sessions::{
    ComponentSource, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionConfigId, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionConfigValueId, SessionModeId, SessionModelId,
};

use crate::copilot::Message;
use crate::storage::SessionState;

struct Agent;

/// Host-side representation of an `agent.session` resource. Dropping it evicts
/// the session's in-memory state.
pub struct ProviderSession {
    id: String,
}

impl Drop for ProviderSession {
    fn drop(&mut self) {
        SESSIONS.with(|s| {
            s.borrow_mut().remove(&self.id);
        });
    }
}

impl GuestSession for ProviderSession {
    async fn prompt(&self, prompt: Vec<ContentBlock>) -> PromptTurn {
        // Allocate the body stream up front. Both halves go into the returned
        // prompt-turn resource; `response()` parks the writer in
        // `ACTIVE_WRITER` while running the prompt loop so `emit_update` finds
        // it, then drops it to EOF the stream.
        let (writer, reader) = acp_wasm_sys::provider::wit_stream::new::<SessionUpdate>();
        PromptTurn::new(ProviderPromptTurn {
            session_id: self.id.clone(),
            inputs: RefCell::new(Some(prompt)),
            writer: RefCell::new(Some(writer)),
            reader: RefCell::new(Some(reader)),
        })
    }

    async fn set_mode(&self, _mode_id: SessionModeId) -> Result<(), Error> {
        // The Copilot API exposes no "mode" concept, so this provider never
        // advertises modes and the legacy `set-mode` slot is unused (it is
        // reserved for layers). Model and thinking-level selection are driven
        // through `set-config-option` instead.
        Err(err(
            ErrorCode::InvalidParams,
            "copilot provider does not support modes",
        ))
    }

    async fn select_model(&self, model_id: SessionModelId) -> Result<(), Error> {
        let session_id = self.id.clone();
        SESSIONS
            .with(|s| {
                let mut sessions = s.borrow_mut();
                match sessions.get_mut(&session_id) {
                    Some(session) => {
                        session.model = model_id;
                        Ok(())
                    }
                    None => Err(format!("unknown session id: {session_id}")),
                }
            })
            .map_err(|e| err(ErrorCode::InvalidParams, &e))
    }

    async fn set_config_option(
        &self,
        config_id: SessionConfigId,
        value: SessionConfigValueId,
    ) -> Result<Vec<SessionConfigOption>, Error> {
        let session_id = self.id.clone();

        // Apply the change to the addressed option, validating the value
        // against the option's advertised (upstream) set.
        let snapshot = SESSIONS
            .with(|s| {
                let mut sessions = s.borrow_mut();
                let Some(session) = sessions.get_mut(&session_id) else {
                    return Err(format!("unknown session id: {session_id}"));
                };
                match config_id.as_str() {
                    CONFIG_MODEL => {
                        // Accept any id the account currently advertises; if
                        // the cache is empty (e.g. the models endpoint was
                        // unreachable) accept optimistically.
                        let known = MODELS_CACHE.with(|c| {
                            let cache = c.borrow();
                            cache.is_empty() || cache.iter().any(|m| m.id == value)
                        });
                        if !known {
                            return Err(format!("unknown model: {value}"));
                        }
                        session.model = value.clone();
                        // The new model may support a different set of
                        // reasoning levels (or none). Re-resolve so we never
                        // carry a level the model rejects.
                        let models = cached_models(&session.model);
                        session.reasoning =
                            resolve_reasoning(find_model(&models, &session.model), &session.reasoning);
                    }
                    CONFIG_REASONING => {
                        // Validate against the current model's upstream
                        // reasoning levels — the only source of truth.
                        let models = cached_models(&session.model);
                        let supported = find_model(&models, &session.model)
                            .map(|m| m.reasoning_efforts.iter().any(|e| *e == value))
                            .unwrap_or(false);
                        if !supported {
                            return Err(format!(
                                "model {} does not support thinking level: {value}",
                                session.model
                            ));
                        }
                        session.reasoning = value.clone();
                    }
                    other => return Err(format!("unknown config option: {other}")),
                }
                Ok(session.clone())
            })
            .map_err(|e| err(ErrorCode::InvalidParams, &e))?;

        // Persistence is best-effort; a failed save shouldn't fail the switch.
        let _ = storage::save(&session_id, &snapshot);

        // Rebuild the full option set from the cached model list — no network.
        let models = cached_models(&snapshot.model);
        Ok(build_config_options(
            &models,
            &snapshot.model,
            &snapshot.reasoning,
        ))
    }
}

/// Owned state for a `prompt-turn` resource. Constructed by
/// [`GuestSession::prompt`]; consumed by either [`updates()`] (which hands out
/// the reader) or [`response()`] (which runs the prompt loop while writing
/// updates into the writer).
pub struct ProviderPromptTurn {
    session_id: String,
    inputs: RefCell<Option<Vec<ContentBlock>>>,
    writer: RefCell<Option<wit_bindgen::rt::async_support::StreamWriter<SessionUpdate>>>,
    reader: RefCell<Option<wit_bindgen::rt::async_support::StreamReader<SessionUpdate>>>,
}

// Single-threaded wasm guest: a thread-local cell is enough.
thread_local! {
    static ACTIVE_WRITER: RefCell<
        Option<wit_bindgen::rt::async_support::StreamWriter<SessionUpdate>>,
    > = const { RefCell::new(None) };
}

impl GuestPromptTurn for ProviderPromptTurn {
    async fn updates(&self) -> wit_bindgen::rt::async_support::StreamReader<SessionUpdate> {
        // First call wins. Subsequent calls get an immediately-EOF empty
        // stream.
        match self.reader.borrow_mut().take() {
            Some(r) => r,
            None => {
                let (_w, r) = acp_wasm_sys::provider::wit_stream::new::<SessionUpdate>();
                r
            }
        }
    }

    async fn response(&self) -> Result<PromptResponse, Error> {
        let inputs = match self.inputs.borrow_mut().take() {
            Some(v) => v,
            None => {
                return Err(err(
                    ErrorCode::InternalError,
                    "prompt-turn.response called twice",
                ));
            }
        };
        let writer = self.writer.borrow_mut().take();
        ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = writer);
        let r = prompt_impl(self.session_id.clone(), inputs).await;
        // Drop the writer so the host-side reader sees end-of-stream.
        ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = None);
        r
    }
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Push a `SessionUpdate` onto the currently-active prompt-turn's body stream.
/// No-op if nothing is active.
async fn emit_update(_session_id: String, update: SessionUpdate) {
    let Some(mut writer) = ACTIVE_WRITER.with(|cell| cell.borrow_mut().take()) else {
        return;
    };
    let (result, _buf) = writer.write(vec![update]).await;
    // If the host closed the reader, drop the writer instead of writing again
    // (which would trap). Skip re-parking so future emits no-op.
    use wit_bindgen::rt::async_support::StreamResult;
    if matches!(result, StreamResult::Dropped | StreamResult::Cancelled) {
        return;
    }
    ACTIVE_WRITER.with(|cell| *cell.borrow_mut() = Some(writer));
}

// Wasm components are single-threaded; thread-local + RefCell avoids any
// synchronization cost while keeping interior mutability.
/// Component id stamped on every config option this provider contributes.
const COMPONENT_ID: &str = "local:copilot-provider";

// Config option ids. Stable identifiers the editor echoes back on
// `set-config-option`.
const CONFIG_MODEL: &str = "model";
const CONFIG_REASONING: &str = "reasoning-effort";

/// Preferred default thinking level, used when a model supports reasoning
/// but the session has no (valid) selection yet. Falls back to the model's
/// first advertised level when `medium` isn't offered.
const PREFERRED_REASONING: &str = "medium";

thread_local! {
    /// The account's chat-capable model list (with per-model capabilities),
    /// cached per host process the first time a session lifecycle response
    /// is built. Lets `set-config-option` rebuild the full option set —
    /// including each model's upstream reasoning-effort levels — without
    /// re-hitting the models endpoint on every switch.
    static MODELS_CACHE: RefCell<Vec<copilot::CopilotModel>> = const { RefCell::new(Vec::new()) };
}

thread_local! {
    static SESSIONS: RefCell<HashMap<String, SessionState>> = RefCell::new(HashMap::new());
}

/// One-time system message inserted at the start of every fresh session.
const SYSTEM_PROMPT: &str = "You are GitHub Copilot, an AI coding assistant connected to the user's editor. Answer concisely and helpfully. When you include code, use fenced code blocks tagged with the language.";

fn component_source() -> ComponentSource {
    ComponentSource {
        component_id: COMPONENT_ID.to_string(),
    }
}

/// Human-readable label for a reasoning-effort id (e.g. `xhigh` -> `XHigh`).
fn reasoning_label(id: &str) -> String {
    match id {
        "none" => "None".to_string(),
        "low" => "Low".to_string(),
        "medium" => "Medium".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "XHigh".to_string(),
        "max" => "Max".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// Locate a model by id in a listing.
fn find_model<'a>(models: &'a [copilot::CopilotModel], id: &str) -> Option<&'a copilot::CopilotModel> {
    models.iter().find(|m| m.id == id)
}

/// Resolve the effective reasoning level for a model, given the session's
/// stored preference. Returns an empty string when the model advertises no
/// reasoning support (so no thinking selector is shown, and no
/// `reasoning_effort` is sent). Otherwise keeps the stored preference if the
/// model still supports it, else prefers [`PREFERRED_REASONING`], else the
/// model's first advertised level.
fn resolve_reasoning(model: Option<&copilot::CopilotModel>, stored: &str) -> String {
    let Some(model) = model else {
        return String::new();
    };
    let efforts = &model.reasoning_efforts;
    if efforts.is_empty() {
        return String::new();
    }
    if efforts.iter().any(|e| e == stored) {
        return stored.to_string();
    }
    if efforts.iter().any(|e| e == PREFERRED_REASONING) {
        return PREFERRED_REASONING.to_string();
    }
    efforts[0].clone()
}

/// Build the config-option selectors advertised on every session lifecycle
/// response and returned from `set-config-option`. The model selector is
/// always present; the thinking-level selector is present only when the
/// current model advertises `reasoning_effort` upstream, and its options are
/// exactly the levels that model supports.
fn build_config_options(
    models: &[copilot::CopilotModel],
    current_model: &str,
    current_reasoning: &str,
) -> Vec<SessionConfigOption> {
    let model_options = models
        .iter()
        .map(|m| SessionConfigSelectOption {
            value: m.id.clone(),
            name: m.name.clone(),
            description: None,
        })
        .collect();
    let mut options = vec![SessionConfigOption {
        id: CONFIG_MODEL.to_string(),
        name: "Model".to_string(),
        description: Some("Which Copilot model backs this session.".to_string()),
        category: Some(SessionConfigOptionCategory::Model),
        current_value: current_model.to_string(),
        options: model_options,
        provided_by: component_source(),
    }];

    // Thinking levels come straight from the current model's upstream
    // capabilities; omit the selector entirely for models without reasoning.
    if let Some(model) = find_model(models, current_model) {
        if !model.reasoning_efforts.is_empty() {
            let reasoning_options = model
                .reasoning_efforts
                .iter()
                .map(|id| SessionConfigSelectOption {
                    value: id.clone(),
                    name: reasoning_label(id),
                    description: None,
                })
                .collect();
            options.push(SessionConfigOption {
                id: CONFIG_REASONING.to_string(),
                name: "Thinking".to_string(),
                description: Some(
                    "How much reasoning effort the model applies (from the model's \
                     capabilities)."
                        .to_string(),
                ),
                category: Some(SessionConfigOptionCategory::ThoughtLevel),
                current_value: current_reasoning.to_string(),
                options: reasoning_options,
                provided_by: component_source(),
            });
        }
    }
    options
}

/// Pick the current model: `preferred` if advertised, else
/// [`copilot::default_model`] if advertised, else the first model listed.
fn pick_current_model(models: &[copilot::CopilotModel], preferred: Option<&str>) -> String {
    let default = copilot::default_model();
    let has = |want: &str| models.iter().any(|m| m.id == want);
    preferred
        .filter(|m| has(m))
        .map(|s| s.to_string())
        .or_else(|| if has(&default) { Some(default.clone()) } else { None })
        .unwrap_or_else(|| models[0].id.clone())
}

/// Read the cached model list, falling back to a single (reasoning-less)
/// entry for `current_model` when the cache is empty (e.g. the models
/// endpoint was unreachable). Keeps `set-config-option` offline.
fn cached_models(current_model: &str) -> Vec<copilot::CopilotModel> {
    let cached = MODELS_CACHE.with(|c| c.borrow().clone());
    if cached.is_empty() {
        vec![copilot::CopilotModel {
            id: current_model.to_string(),
            name: current_model.to_string(),
            reasoning_efforts: Vec::new(),
        }]
    } else {
        cached
    }
}

/// List chat-capable Copilot models (populating [`MODELS_CACHE`]) and build the
/// session's config-option selectors, the resolved current model, and the
/// resolved thinking level for that model. Falls back to a single entry for
/// [`copilot::default_model`] if the API is unreachable or returns nothing, so
/// session creation still succeeds.
async fn build_session_config(
    preferred_model: Option<&str>,
    stored_reasoning: &str,
) -> (Vec<SessionConfigOption>, String, String) {
    let default = copilot::default_model();
    let listed = match copilot::list_models().await {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            eprintln!("copilot returned no models; using default");
            vec![copilot::CopilotModel {
                id: default.clone(),
                name: default.clone(),
                reasoning_efforts: Vec::new(),
            }]
        }
        Err(e) => {
            eprintln!("failed to list copilot models ({e}); using default");
            vec![copilot::CopilotModel {
                id: default.clone(),
                name: default.clone(),
                reasoning_efforts: Vec::new(),
            }]
        }
    };
    MODELS_CACHE.with(|c| *c.borrow_mut() = listed.clone());
    let current = pick_current_model(&listed, preferred_model);
    let reasoning = resolve_reasoning(find_model(&listed, &current), stored_reasoning);
    let options = build_config_options(&listed, &current, &reasoning);
    (options, current, reasoning)
}

fn next_session_id() -> String {
    // Mix wall-clock seconds with a per-process counter so ids are unique
    // across host restarts.
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("copilot-session-{secs:x}-{n}")
}

fn err(code: ErrorCode, message: &str) -> Error {
    Error {
        code,
        message: message.to_string(),
    }
}

/// Pull all `text` content blocks from a prompt, joined with blank lines.
fn extract_user_text(prompt: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in prompt {
        if let ContentBlock::Text(TextContent { text }) = block {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
    }
    out
}

impl Guest for Agent {
    type Session = ProviderSession;
    type PromptTurn = ProviderPromptTurn;

    async fn initialize(_req: InitializeRequest) -> Result<InitializeResponse, Error> {
        Ok(InitializeResponse {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                },
                mcp_capabilities: McpCapabilities {
                    http: false,
                    sse: false,
                },
                session_capabilities: SessionCapabilities {
                    list: false,
                    resume: true,
                    close: false,
                },
            },
            agent_info: Some(ImplementationInfo {
                name: "copilot-wasm-agent".to_string(),
                title: Some("GitHub Copilot (wasm)".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            auth_methods: Vec::new(),
        })
    }

    async fn authenticate(_req: AuthenticateRequest) -> Result<(), Error> {
        // Auth is handled out-of-band via the host secrets store / env; there
        // is no interactive auth method to run here.
        Err(err(
            ErrorCode::MethodNotFound,
            "authentication not required; configure a GitHub token via the host secrets store",
        ))
    }

    async fn new_session(req: NewSessionRequest) -> Result<(Session, NewSessionResponse), Error> {
        let id = next_session_id();
        let (config_options, current_model, reasoning) =
            build_session_config(None, PREFERRED_REASONING).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                id.clone(),
                SessionState {
                    history: Vec::new(),
                    model: current_model,
                    reasoning,
                    cwd: req.cwd,
                },
            )
        });
        let resource = Session::new(ProviderSession { id: id.clone() });
        Ok((
            resource,
            NewSessionResponse {
                session_id: id,
                modes: None,
                models: None,
                config_options: Some(config_options),
            },
        ))
    }

    async fn load_session(
        req: LoadSessionRequest,
    ) -> Result<(Session, LoadSessionResponse), Error> {
        let session_id = req.session_id.clone();
        // Load the persisted session if present, then replay history to the
        // client as `update-session` notifications (per the ACP spec for
        // `session/load`). Missing file = fresh session.
        let default = copilot::default_model();
        let stored = match storage::load(&session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("load: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let stored_reasoning = stored
            .as_ref()
            .map(|s| s.reasoning.clone())
            .unwrap_or_default();
        let (config_options, current_model, reasoning) =
            build_session_config(preferred.as_deref(), &stored_reasoning).await;
        for msg in &history {
            let block = ContentBlock::Text(TextContent {
                text: msg.content.clone(),
            });
            let update = match msg.role.as_str() {
                "user" => SessionUpdate::UserMessageChunk(block),
                "assistant" => SessionUpdate::AgentMessageChunk(block),
                _ => continue,
            };
            emit_update(session_id.clone(), update).await;
        }
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                session_id.clone(),
                SessionState {
                    history,
                    model: current_model,
                    reasoning,
                    cwd: req.cwd,
                },
            );
        });
        let resource = Session::new(ProviderSession { id: session_id });
        Ok((
            resource,
            LoadSessionResponse {
                modes: None,
                models: None,
                config_options: Some(config_options),
            },
        ))
    }

    async fn list_sessions(_req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        Err(err(ErrorCode::MethodNotFound, "list-sessions not supported"))
    }

    async fn resume_session(
        req: ResumeSessionRequest,
    ) -> Result<(Session, ResumeSessionResponse), Error> {
        let session_id = req.session_id.clone();
        // Like `load_session`, but the spec forbids replaying history via
        // `update-session`. Just rehydrate the in-memory map.
        let default = copilot::default_model();
        let stored = match storage::load(&session_id, &default) {
            Ok(s) => s,
            Err(e) => return Err(err(ErrorCode::InternalError, &format!("resume: {e}"))),
        };
        let history = stored
            .as_ref()
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let preferred = stored.as_ref().map(|s| s.model.clone());
        let stored_reasoning = stored
            .as_ref()
            .map(|s| s.reasoning.clone())
            .unwrap_or_default();
        let (config_options, current_model, reasoning) =
            build_session_config(preferred.as_deref(), &stored_reasoning).await;
        SESSIONS.with(|s| {
            s.borrow_mut().insert(
                session_id.clone(),
                SessionState {
                    history,
                    model: current_model,
                    reasoning,
                    cwd: req.cwd,
                },
            );
        });
        let resource = Session::new(ProviderSession { id: session_id });
        Ok((
            resource,
            ResumeSessionResponse {
                modes: None,
                models: None,
                config_options: Some(config_options),
            },
        ))
    }
}

async fn prompt_impl(
    session_id: String,
    prompt: Vec<ContentBlock>,
) -> Result<PromptResponse, Error> {
    let user_text = extract_user_text(&prompt);
    if user_text.is_empty() {
        return Err(err(
            ErrorCode::InvalidParams,
            "prompt contained no text content",
        ));
    }

    // Append the user turn to this session's history and grab a copy (plus the
    // active model and thinking level) to send to Copilot. New sessions can
    // land here without going through `new-session` (e.g. tests); fall back to
    // defaults. A one-time `system` message is prepended on the first prompt.
    let (history, model, reasoning) = SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        let entry = sessions.entry(session_id.clone()).or_insert_with(|| SessionState {
            history: Vec::new(),
            model: copilot::default_model(),
            reasoning: String::new(),
            cwd: String::new(),
        });
        if entry.history.is_empty() {
            let mut prompt = SYSTEM_PROMPT.to_string();
            if !entry.cwd.is_empty() {
                prompt.push_str("\n\nThe user's current project directory is `");
                prompt.push_str(&entry.cwd);
                prompt.push_str("`. Resolve relative file references against it.");
            }
            entry.history.push(Message::system(prompt));
        }
        entry.history.push(Message::user(user_text.clone()));
        (entry.history.clone(), entry.model.clone(), entry.reasoning.clone())
    });

    // Apply the thinking level as the model's native `reasoning_effort`
    // parameter — but only when the selected model actually advertises that
    // level upstream, so we never send it to a model that would reject it
    // (e.g. gpt-4o returns `invalid_reasoning_effort`).
    let effort = if reasoning.is_empty() {
        None
    } else {
        let models = cached_models(&model);
        let supported = find_model(&models, &model)
            .map(|m| m.reasoning_efforts.iter().any(|e| *e == reasoning))
            .unwrap_or(false);
        supported.then(|| reasoning.clone())
    };

    let session_id_chunk = session_id.clone();
    let reply = copilot::chat(model, effort, history, |chunk| {
        let sid = session_id_chunk.clone();
        async move {
            emit_update(
                sid,
                SessionUpdate::AgentMessageChunk(ContentBlock::Text(TextContent { text: chunk })),
            )
            .await;
        }
    })
    .await
    .map_err(|e| err(ErrorCode::InternalError, &format!("copilot: {e}")))?;

    // Append the assistant reply to history and persist.
    let snapshot = SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        sessions.get_mut(&session_id).map(|entry| {
            entry.history.push(Message::assistant(reply.clone()));
            entry.clone()
        })
    });
    if let Some(session) = snapshot {
        if let Err(e) = storage::save(&session_id, &session) {
            // Persistence is best-effort: a failed save shouldn't fail the
            // prompt turn. Surface it as a thought chunk.
            emit_update(
                session_id.clone(),
                SessionUpdate::AgentThoughtChunk(ContentBlock::Text(TextContent {
                    text: format!("(failed to persist session: {e})"),
                })),
            )
            .await;
        }
    }

    Ok(PromptResponse {
        stop_reason: StopReason::EndTurn,
    })
}

acp_wasm_sys::provider::export!(Agent with_types_in acp_wasm_sys::provider);
