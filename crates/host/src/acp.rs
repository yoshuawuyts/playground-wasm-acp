//! Single-namespace re-exports of the WIT types we use.
//!
//! In the 2.0.x ACP package everything lived under one `types` interface;
//! 3.0.0 split it into many. Rather than scatter `crate::yoshuawuyts::acp::sessions::*`
//! imports throughout the host, this module pulls each type into one flat
//! `crate::acp` namespace.
//!
//! Intentionally a re-export shim, not a separate type set. If a type name
//! collides across interfaces (it shouldn't — ACP names are unique), expose
//! it qualified instead of papering over.
//!
//! Some re-exports are unused today; they're kept as a discoverable index of
//! what's available when adding new translation paths.
#![allow(unused_imports)]

pub use crate::yoshuawuyts::acp::content::{
    AudioContent, ContentBlock, EmbeddedResource, ImageContent, ResourceContents, ResourceLink,
    TextContent, TextResourceContents,
};
pub use crate::yoshuawuyts::acp::errors::{Error, ErrorCode};
pub use crate::yoshuawuyts::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
pub use crate::yoshuawuyts::acp::init::{
    AgentCapabilities, AuthenticateRequest, ClientCapabilities, FsCapabilities, ImplementationInfo,
    InitializeRequest, InitializeResponse, McpCapabilities, PromptCapabilities,
    SessionCapabilities,
};
pub use crate::yoshuawuyts::acp::prompts::{
    PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
pub use crate::yoshuawuyts::acp::sessions::{
    EnvVar, HttpHeader, LoadSessionRequest, LoadSessionResponse, McpServer, McpServerHttp,
    McpServerSse, McpServerStdio, NewSessionRequest, NewSessionResponse, SessionId,
};
pub use crate::yoshuawuyts::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
pub use crate::yoshuawuyts::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};
