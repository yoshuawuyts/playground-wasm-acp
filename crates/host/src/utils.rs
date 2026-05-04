//! Small shared helpers for the host binary.

use anyhow::Result;

/// Derive a component id from the wasm path. We use the file stem; renaming
/// the binary therefore loses prior data (acceptable for a sample, and a
/// future `--component-id` flag can override). Restricted to a small
/// alphabet to avoid surprising filesystem behaviour.
pub fn component_id_from_path(path: &std::path::Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("wasm path has no usable file stem: {}", path.display()))?;
    let ok = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        anyhow::bail!(
            "wasm filename stem {stem:?} contains characters not allowed in a component id (allow [A-Za-z0-9._-])"
        );
    }
    Ok(stem.to_string())
}
