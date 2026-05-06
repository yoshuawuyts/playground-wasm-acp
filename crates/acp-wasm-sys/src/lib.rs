//! Auto-generated WIT bindings for the `yosh:acp` worlds.
//!
//! Two worlds, two submodules:
//!
//! * [`provider`] — terminal ACP agent. Exports `agent`, imports `client`.
//!   Used by the `ollama-provider` crate.
//! * [`layer`] — bidirectional ACP middleware. Exports both `agent` and
//!   `client`, imports both. Used by the `uppercase-layer` crate.
//!
//! Both bindings are regenerated from `vendor/wit/*.wit` by
//! `just bindgen` (or the per-world `just bindgen-provider` /
//! `just bindgen-layer`). Do not edit `provider.rs` / `layer.rs` by
//! hand — changes will be overwritten.
//!
//! The two binding files duplicate the shared interface types
//! (`errors`, `sessions`, `content`, …); each consumer crate only pulls
//! in one submodule, so the duplication has no impact at link time
//! within a single wasm component.

#![allow(clippy::all)]

pub mod layer;
pub mod provider;
