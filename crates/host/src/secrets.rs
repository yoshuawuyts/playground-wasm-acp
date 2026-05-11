//! Secret provisioning: host-side `wasmcloud:secrets@0.1.0-draft` backend.
//!
//! Secrets are loaded from a TOML config file passed via `--secrets`.
//! Lookups are scoped by component id: each stage in the chain can only
//! read keys that the operator has explicitly granted to that component
//! id (deny-by-default).
//!
//! Each entry is either an inline `value = "…"` (plaintext UTF-8 string,
//! or base64-decoded bytes if `bytes = true`) or a `command = ["op",
//! "read", "op://..."]` whose stdout is captured at first read and
//! cached for the host process lifetime. Resolved values never appear in
//! logs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Spec-aligned error type. Mirrors `wasmcloud:secrets/store.secrets-error`.
#[derive(Debug)]
pub enum SecretsError {
    /// Backend (command) returned non-zero or unparseable output.
    Upstream(String),
    /// I/O failure invoking the backend (spawn error, timeout, etc.).
    Io(String),
    /// Key not granted to this component id.
    NotFound,
}

/// Spec-aligned value type. Mirrors `wasmcloud:secrets/store.secret-value`.
/// `Debug` is redacted so it never leaks via logs.
#[derive(Clone)]
pub enum SecretValue {
    String(String),
    Bytes(Vec<u8>),
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretValue::String(_) => f.write_str("SecretValue::String(<redacted>)"),
            SecretValue::Bytes(_) => f.write_str("SecretValue::Bytes(<redacted>)"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    bytes: bool,
    #[serde(default, rename = "timeout-ms")]
    timeout_ms: Option<u64>,
}

#[derive(Debug)]
enum Source {
    Plaintext(SecretValue),
    Command {
        program: String,
        args: Vec<String>,
        timeout: Duration,
        bytes: bool,
    },
}

/// Parsed config: `component-id -> key -> entry`.
pub struct SecretsRegistry {
    entries: HashMap<String, HashMap<String, Source>>,
    cache: Mutex<HashMap<(String, String), SecretValue>>,
}

impl SecretsRegistry {
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Load and validate a TOML config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading secrets config {}", path.display()))?;
        let raw: HashMap<String, HashMap<String, RawEntry>> = toml::from_str(&text)
            .with_context(|| format!("parsing secrets config {}", path.display()))?;

        let mut entries: HashMap<String, HashMap<String, Source>> = HashMap::new();
        for (component_id, keys) in raw {
            let mut by_key = HashMap::new();
            for (key, entry) in keys {
                let source = match (entry.value, entry.command) {
                    (Some(_), Some(_)) => anyhow::bail!(
                        "secrets[{component_id}][{key}]: cannot set both `value` and `command`"
                    ),
                    (None, None) => anyhow::bail!(
                        "secrets[{component_id}][{key}]: must set either `value` or `command`"
                    ),
                    (Some(v), None) => {
                        let val = if entry.bytes {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(v.as_bytes())
                                .with_context(|| {
                                    format!(
                                        "secrets[{component_id}][{key}]: invalid base64 for `bytes`"
                                    )
                                })?;
                            SecretValue::Bytes(bytes)
                        } else {
                            SecretValue::String(v)
                        };
                        Source::Plaintext(val)
                    }
                    (None, Some(cmd)) => {
                        let mut iter = cmd.into_iter();
                        let program = iter.next().with_context(|| {
                            format!("secrets[{component_id}][{key}]: empty `command`")
                        })?;
                        let args: Vec<String> = iter.collect();
                        let timeout = entry
                            .timeout_ms
                            .map(Duration::from_millis)
                            .unwrap_or(DEFAULT_COMMAND_TIMEOUT);
                        Source::Command {
                            program,
                            args,
                            timeout,
                            bytes: entry.bytes,
                        }
                    }
                };
                by_key.insert(key, source);
            }
            entries.insert(component_id, by_key);
        }

        Ok(Self {
            entries,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn cache_get(&self, component_id: &str, key: &str) -> Option<SecretValue> {
        self.cache
            .lock()
            .ok()?
            .get(&(component_id.to_string(), key.to_string()))
            .cloned()
    }

    fn cache_put(&self, component_id: &str, key: &str, value: SecretValue) {
        if let Ok(mut g) = self.cache.lock() {
            g.insert((component_id.to_string(), key.to_string()), value);
        }
    }

    /// Resolve a secret. Deny-by-default if the component id has no entry
    /// or the key isn't granted.
    pub async fn resolve(
        &self,
        component_id: &str,
        key: &str,
    ) -> Result<SecretValue, SecretsError> {
        let source = self
            .entries
            .get(component_id)
            .and_then(|m| m.get(key))
            .ok_or(SecretsError::NotFound)?;

        match source {
            Source::Plaintext(v) => Ok(v.clone()),
            Source::Command {
                program,
                args,
                timeout: t,
                bytes,
            } => {
                if let Some(v) = self.cache_get(component_id, key) {
                    return Ok(v);
                }
                let mut cmd = Command::new(program);
                cmd.args(args);
                let fut = cmd.output();
                let output = match timeout(*t, fut).await {
                    Err(_) => {
                        return Err(SecretsError::Io(format!(
                            "command `{program}` timed out after {:?}",
                            t
                        )));
                    }
                    Ok(Err(e)) => return Err(SecretsError::Io(format!("spawn: {e}"))),
                    Ok(Ok(o)) => o,
                };
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(SecretsError::Upstream(format!(
                        "command `{program}` exited {}: {}",
                        output.status,
                        stderr.trim()
                    )));
                }
                let value = if *bytes {
                    SecretValue::Bytes(output.stdout)
                } else {
                    let mut s = String::from_utf8(output.stdout).map_err(|e| {
                        SecretsError::Upstream(format!("command output not UTF-8: {e}"))
                    })?;
                    if s.ends_with('\n') {
                        s.pop();
                        if s.ends_with('\r') {
                            s.pop();
                        }
                    }
                    SecretValue::String(s)
                };
                self.cache_put(component_id, key, value.clone());
                Ok(value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn write_config(text: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn empty_registry_not_found() {
        let r = SecretsRegistry::empty();
        assert!(matches!(
            r.resolve("c", "k").await,
            Err(SecretsError::NotFound)
        ));
    }

    #[tokio::test]
    async fn plaintext_string() {
        let f = write_config(
            r#"
[component_a]
api_key = { value = "hunter2" }
"#,
        );
        let r = SecretsRegistry::load(f.path()).unwrap();
        match r.resolve("component_a", "api_key").await.unwrap() {
            SecretValue::String(s) => assert_eq!(s, "hunter2"),
            _ => panic!("expected string"),
        }
    }

    #[tokio::test]
    async fn plaintext_bytes_base64() {
        let f = write_config(
            r#"
[component_a]
blob = { value = "aGVsbG8=", bytes = true }
"#,
        );
        let r = SecretsRegistry::load(f.path()).unwrap();
        match r.resolve("component_a", "blob").await.unwrap() {
            SecretValue::Bytes(b) => assert_eq!(b, b"hello"),
            _ => panic!("expected bytes"),
        }
    }

    #[tokio::test]
    async fn cross_component_isolation() {
        let f = write_config(
            r#"
[component_a]
shared = { value = "for-a" }
"#,
        );
        let r = SecretsRegistry::load(f.path()).unwrap();
        assert!(matches!(
            r.resolve("component_b", "shared").await,
            Err(SecretsError::NotFound)
        ));
    }

    #[tokio::test]
    async fn command_success_and_cached() {
        // Counter script: appends to a file, prints incrementing value.
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        std::fs::write(&counter, "0").unwrap();
        let script = dir.path().join("c.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nn=$(cat {p})\necho $((n+1)) > {p}\necho secret-v$n\n",
                p = counter.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();

        let cfg = format!(
            r#"
[c]
k = {{ command = ["{}"] }}
"#,
            script.display()
        );
        let f = write_config(&cfg);
        let r = Arc::new(SecretsRegistry::load(f.path()).unwrap());
        let first = r.resolve("c", "k").await.unwrap();
        let second = r.resolve("c", "k").await.unwrap();
        match (first, second) {
            (SecretValue::String(a), SecretValue::String(b)) => {
                assert_eq!(a, b, "cache should return identical value");
                assert_eq!(a, "secret-v0");
            }
            _ => panic!("expected string"),
        }
    }

    #[tokio::test]
    async fn command_nonzero_is_upstream_error() {
        let f = write_config(
            r#"
[c]
k = { command = ["false"] }
"#,
        );
        let r = SecretsRegistry::load(f.path()).unwrap();
        assert!(matches!(
            r.resolve("c", "k").await,
            Err(SecretsError::Upstream(_))
        ));
    }

    #[tokio::test]
    async fn command_spawn_failure_is_io() {
        let f = write_config(
            r#"
[c]
k = { command = ["/nonexistent/definitely-not-here"] }
"#,
        );
        let r = SecretsRegistry::load(f.path()).unwrap();
        assert!(matches!(
            r.resolve("c", "k").await,
            Err(SecretsError::Io(_))
        ));
    }

    #[tokio::test]
    async fn rejects_both_value_and_command() {
        let f = write_config(
            r#"
[c]
k = { value = "x", command = ["echo", "y"] }
"#,
        );
        assert!(SecretsRegistry::load(f.path()).is_err());
    }
}
