//! Per-session conversation history persistence.
//!
//! The host gives every wasm instance a private read/write directory at
//! `/data` (a per-app scratch dir). This component organises its own
//! `sessions/` subdirectory inside it and writes one JSON file per
//! session: `/data/sessions/<id>.json`.
//!
//! Other components are free to use `/data` differently — caches,
//! embeddings, model state — or to ignore it entirely (e.g. components
//! backed by a remote API that owns its own persistence).

use std::fs;
use std::path::PathBuf;

use crate::ollama::Message;

const ROOT: &str = "/data";
const SESSIONS_SUBDIR: &str = "sessions";

fn sessions_dir() -> PathBuf {
    PathBuf::from(format!("{ROOT}/{SESSIONS_SUBDIR}"))
}

/// Sanitize a session id into a filename. Session ids are agent-minted but
/// might contain path separators in the future; reject anything that isn't
/// a plain filename component.
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

/// Read history from disk. Returns `Ok(None)` if the file doesn't exist
/// (a fresh session); `Err` only on real I/O or decode failures.
pub fn load(session_id: &str) -> Result<Option<Vec<Message>>, String> {
    let Some(path) = path_for(session_id) else {
        return Err(format!("invalid session id: {session_id:?}"));
    };
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("decode {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Persist history to disk. Creates the `sessions/` subdir if missing and
/// overwrites any existing file.
pub fn save(session_id: &str, history: &[Message]) -> Result<(), String> {
    let Some(path) = path_for(session_id) else {
        return Err(format!("invalid session id: {session_id:?}"));
    };
    fs::create_dir_all(sessions_dir())
        .map_err(|e| format!("mkdir {}: {e}", sessions_dir().display()))?;
    let bytes =
        serde_json::to_vec_pretty(history).map_err(|e| format!("encode session: {e}"))?;
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

