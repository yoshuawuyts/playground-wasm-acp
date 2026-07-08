//! Per-session conversation history persistence.
//!
//! The host gives every wasm instance a private read/write directory at
//! `/data` (a per-app, per-project scratch dir). This component writes one
//! JSON file per session under `/data/sessions/<id>.json`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::copilot::Message;

const ROOT: &str = "/data";
const SESSIONS_SUBDIR: &str = "sessions";

fn sessions_dir() -> PathBuf {
    PathBuf::from(format!("{ROOT}/{SESSIONS_SUBDIR}"))
}

/// Sanitize a session id into a filename. Reject anything that isn't a plain
/// filename component.
fn path_for(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id == "."
        || session_id == ".."
    {
        return None;
    }
    Some(sessions_dir().join(format!("{session_id}.json")))
}

/// On-disk session payload: conversation history plus the active model, the
/// active thinking level, and the working directory the editor sent on
/// `session/new` / `session/load`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub history: Vec<Message>,
    pub model: String,
    /// Active thinking level id (e.g. `"low"`, `"medium"`, `"high"`), sourced
    /// from the current model's upstream `reasoning_effort` set. Empty when the
    /// model advertises no reasoning levels. Sent as the native
    /// `reasoning_effort` API parameter on each turn.
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub cwd: String,
    /// Cumulative premium-request cost billed to this session so far, in
    /// premium-request units (model `billing.multiplier` summed over model
    /// API turns on premium models). Reported to the editor via the
    /// `usage-update`'s cost field so users can track spend. `0` for sessions
    /// that have only used included (non-premium) models.
    #[serde(default)]
    pub premium_requests: f64,
}

/// Read a session from disk. Returns `Ok(None)` if the file doesn't exist.
pub fn load(session_id: &str, default_model: &str) -> Result<Option<SessionState>, String> {
    let Some(path) = path_for(session_id) else {
        return Err(format!("invalid session id: {session_id:?}"));
    };
    match fs::read(&path) {
        Ok(bytes) => {
            let mut session: SessionState = serde_json::from_slice(&bytes)
                .map_err(|e| format!("decode {}: {e}", path.display()))?;
            if session.model.is_empty() {
                session.model = default_model.to_string();
            }
            Ok(Some(session))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Persist a session to disk. Creates the `sessions/` subdir if missing and
/// overwrites any existing file.
pub fn save(session_id: &str, session: &SessionState) -> Result<(), String> {
    let Some(path) = path_for(session_id) else {
        return Err(format!("invalid session id: {session_id:?}"));
    };
    fs::create_dir_all(sessions_dir())
        .map_err(|e| format!("mkdir {}: {e}", sessions_dir().display()))?;
    let bytes = serde_json::to_vec_pretty(session).map_err(|e| format!("encode session: {e}"))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}
