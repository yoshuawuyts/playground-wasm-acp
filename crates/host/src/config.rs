//! Global host configuration loaded from an XDG config file.
//!
//! The host reads a single global TOML config at startup (no CLI flag).
//! Its location honours `$XDG_CONFIG_HOME` when set, otherwise falls back
//! to [`dirs::config_dir()`] — mirroring the layout
//! [`crate::install::cache_root`] uses for the component cache:
//!
//! ```text
//! $XDG_CONFIG_HOME/acp-wasm/config.toml
//! ```
//!
//! Currently the only thing it configures is **filesystem mounts**:
//! additional writable preopens exposed to the agent chain at `/<name>`,
//! alongside the built-in host-backed `/data` preopen. Each mount is
//! either a host directory (`path`) or a `wasi:filesystem`-exporting wasm
//! component (`component`):
//!
//! ```toml
//! [mounts.scratch]
//! path = "/tmp/acp-scratch"
//!
//! [mounts.onedrive]
//! component = "acme:onedrive-fs"
//! ```
//!
//! An absent config file is equivalent to an empty config (no mounts),
//! so the default `/data`-only behaviour is preserved byte-for-byte.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// Mount name reserved for the built-in host-backed preopen.
pub const RESERVED_DATA_MOUNT: &str = "data";

/// Where a mount's contents come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A host directory, preopened directly (served by wasmtime-wasi).
    Path(PathBuf),
    /// A `wasi:filesystem`-exporting wasm component, referenced by
    /// filesystem path or WIT name (resolved via [`crate::install::resolve`]).
    Component(String),
}

/// A single configured filesystem mount, exposed to the chain at
/// `/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountConfig {
    /// Single-segment mount name; the preopen path is `/<name>`.
    pub name: String,
    pub source: MountSource,
}

impl MountConfig {
    /// The guest-visible preopen path for this mount (`/<name>`).
    pub fn guest_path(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Parsed global host config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Filesystem mounts, ordered by mount name for deterministic boot
    /// logging and preopen ordering.
    pub mounts: Vec<MountConfig>,
}

/// Raw `[mounts.<name>]` table: exactly one of `path` / `component`.
#[derive(Debug, Deserialize)]
struct RawMount {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    component: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    mounts: std::collections::HashMap<String, RawMount>,
}

impl Config {
    /// Empty config — no mounts.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Location of the global config file. Honours `$XDG_CONFIG_HOME`
    /// when it is an absolute path, else falls back to
    /// [`dirs::config_dir`] (e.g. `~/.config` on Linux,
    /// `~/Library/Application Support` on macOS).
    pub fn config_path() -> Result<PathBuf> {
        if let Some(val) = std::env::var_os("XDG_CONFIG_HOME") {
            let p = PathBuf::from(val);
            if p.is_absolute() {
                return Ok(p.join("acp-wasm").join("config.toml"));
            }
        }
        let base = dirs::config_dir()
            .ok_or_else(|| anyhow!("cannot determine user config dir for host config"))?;
        Ok(base.join("acp-wasm").join("config.toml"))
    }

    /// Load the config from the default [`Self::config_path`]. A missing
    /// file yields an empty config.
    pub fn load_default() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load(&path)
    }

    /// Load and validate the config from `path`. A missing file yields
    /// an empty config; any other read error is propagated.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading host config {}", path.display()));
            }
        };
        Self::parse(&text).with_context(|| format!("parsing host config {}", path.display()))
    }

    /// Parse and validate config from TOML text.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;

        let mut mounts = Vec::with_capacity(raw.mounts.len());
        for (name, raw_mount) in raw.mounts {
            validate_mount_name(&name)?;
            let source = match (raw_mount.path, raw_mount.component) {
                (Some(_), Some(_)) => {
                    bail!("mounts.{name}: set exactly one of `path` or `component`, not both")
                }
                (None, None) => {
                    bail!("mounts.{name}: must set either `path` or `component`")
                }
                (Some(p), None) => {
                    if p.trim().is_empty() {
                        bail!("mounts.{name}: `path` must not be empty");
                    }
                    MountSource::Path(PathBuf::from(p))
                }
                (None, Some(c)) => {
                    if c.trim().is_empty() {
                        bail!("mounts.{name}: `component` must not be empty");
                    }
                    MountSource::Component(c)
                }
            };
            mounts.push(MountConfig { name, source });
        }

        // Deterministic order so boot logs and preopen ordering are stable.
        mounts.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { mounts })
    }

    /// Component-backed mounts only.
    pub fn component_mounts(&self) -> impl Iterator<Item = &MountConfig> {
        self.mounts
            .iter()
            .filter(|m| matches!(m.source, MountSource::Component(_)))
    }

    /// `true` when at least one mount is component-backed (which engages
    /// the host's `wasi:filesystem` dispatcher).
    pub fn has_component_mounts(&self) -> bool {
        self.component_mounts().next().is_some()
    }
}

/// A mount name must be a single, non-empty path segment so it maps to a
/// `/<name>` preopen cleanly, and must not collide with the reserved
/// built-in `data` mount.
fn validate_mount_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("mount name must not be empty");
    }
    if name == RESERVED_DATA_MOUNT {
        bail!("mount name `{name}` is reserved for the built-in host `/data` preopen");
    }
    if name == "." || name == ".." {
        bail!("mount name `{name}` is not a valid path segment");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("mount name `{name}` must be a single path segment (no `/` or `\\`)");
    }
    if name.contains(['\0', ':']) {
        bail!("mount name `{name}` contains an invalid character");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_empty_config() {
        assert_eq!(Config::parse("").unwrap(), Config::empty());
    }

    #[test]
    fn path_and_component_mounts_parse() {
        let cfg = Config::parse(
            r#"
            [mounts.scratch]
            path = "/tmp/acp-scratch"

            [mounts.onedrive]
            component = "acme:onedrive-fs"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mounts.len(), 2);
        // Sorted by name: onedrive < scratch.
        assert_eq!(cfg.mounts[0].name, "onedrive");
        assert_eq!(
            cfg.mounts[0].source,
            MountSource::Component("acme:onedrive-fs".into())
        );
        assert_eq!(cfg.mounts[1].name, "scratch");
        assert_eq!(
            cfg.mounts[1].source,
            MountSource::Path(PathBuf::from("/tmp/acp-scratch"))
        );
        assert_eq!(cfg.mounts[1].guest_path(), "/scratch");
        assert!(cfg.has_component_mounts());
    }

    #[test]
    fn path_only_config_has_no_component_mounts() {
        let cfg = Config::parse("[mounts.scratch]\npath = \"/tmp/x\"\n").unwrap();
        assert!(!cfg.has_component_mounts());
    }

    #[test]
    fn both_path_and_component_is_error() {
        let err = Config::parse(
            "[mounts.x]\npath = \"/tmp/x\"\ncomponent = \"a:b\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one"), "got: {err}");
    }

    #[test]
    fn neither_path_nor_component_is_error() {
        let err = Config::parse("[mounts.x]\n").unwrap_err().to_string();
        assert!(err.contains("either"), "got: {err}");
    }

    #[test]
    fn reserved_data_name_is_error() {
        let err = Config::parse("[mounts.data]\npath = \"/tmp/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn slash_in_name_is_error() {
        let err = Config::parse("[mounts.\"a/b\"]\npath = \"/tmp/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("single path segment"), "got: {err}");
    }

    #[test]
    fn dotdot_name_is_error() {
        let err = Config::parse("[mounts.\"..\"]\npath = \"/tmp/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("valid path segment"), "got: {err}");
    }

    #[test]
    fn empty_path_value_is_error() {
        let err = Config::parse("[mounts.x]\npath = \"\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn config_path_honours_xdg_config_home() {
        // Only assert the suffix so the test is environment-independent.
        // SAFETY: single-threaded test process mutating its own env.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/xdg-test-home");
        }
        let p = Config::config_path().unwrap();
        assert_eq!(p, PathBuf::from("/xdg-test-home/acp-wasm/config.toml"));
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }
}
