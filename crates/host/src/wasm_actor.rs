//! Channel-driven actor wrapping a wasm component instance.
//!
//! Each chain stage owns one [`WasmActor`]: a persistent
//! `Store::run_concurrent` event loop that pulls [`Cmd`]s off an mpsc
//! channel and dispatches each via `accessor.spawn` so calls execute
//! concurrently inside one store. This replaces the previous
//! `Arc<Mutex<WasmAgent>>` model, whose lock spanned `run_concurrent` and
//! deadlocked when callbacks re-entered.
//!
//! Ownership: the chain factory hands the head [`WasmActor`] to the
//! `SessionActor`. Each stage's `HostState` holds a [`WasmActor`] of its
//! downstream (strong) and a [`WasmActorWeak`] of its upstream. The weak
//! upstream is required to break the cycle between paired stages.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use wasmtime::Store;
use wasmtime::component::{Accessor, AccessorTask, HasSelf};

use crate::state::HostState;
use crate::yosh::acp::errors::Error;
use crate::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use crate::yosh::acp::init::{AuthenticateRequest, InitializeRequest, InitializeResponse};
use crate::yosh::acp::prompts::{PromptRequest, PromptResponse, SessionUpdate};
use crate::yosh::acp::sessions::{
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, ResumeSessionRequest, ResumeSessionResponse,
    SelectModelRequest, SessionId, SetSessionModeRequest,
};
use crate::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use crate::yosh::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};
use crate::{Layer, Provider};

/// Either a terminal provider or an intermediate layer instance.
///
/// Both variants expose the same `agent` interface; only `Layer` exposes
/// `client`. Wrapped in `Arc` so handler tasks (`'static + Send`
/// `AccessorTask`s) can hold their own clones.
pub enum Bindings {
    Provider(Provider),
    Layer(Layer),
}

// -----------------------------------------------------------------------------
// Cmd enum — one variant per wasm export the host invokes.
// -----------------------------------------------------------------------------

type Reply<T> = oneshot::Sender<wasmtime::Result<Result<T, Error>>>;
type ReplyTrap<T> = oneshot::Sender<wasmtime::Result<T>>;

pub enum Cmd {
    // agent direction
    Initialize {
        req: InitializeRequest,
        reply: Reply<InitializeResponse>,
    },
    Authenticate {
        req: AuthenticateRequest,
        reply: Reply<()>,
    },
    NewSession {
        req: NewSessionRequest,
        reply: Reply<NewSessionResponse>,
    },
    LoadSession {
        req: LoadSessionRequest,
        reply: Reply<LoadSessionResponse>,
    },
    ListSessions {
        req: ListSessionsRequest,
        reply: Reply<ListSessionsResponse>,
    },
    ResumeSession {
        req: ResumeSessionRequest,
        reply: Reply<ResumeSessionResponse>,
    },
    SetSessionMode {
        req: SetSessionModeRequest,
        reply: Reply<()>,
    },
    SelectModel {
        req: SelectModelRequest,
        reply: Reply<()>,
    },
    Prompt {
        req: PromptRequest,
        reply: Reply<PromptResponse>,
    },
    CloseSession {
        session_id: SessionId,
        reply: Reply<()>,
    },
    Cancel {
        session_id: SessionId,
        reply: ReplyTrap<()>,
    },
    // client direction (only valid on Layer stages)
    UpdateSession {
        session_id: SessionId,
        update: SessionUpdate,
        reply: ReplyTrap<()>,
    },
    RequestPermission {
        req: RequestPermissionRequest,
        reply: Reply<RequestPermissionResponse>,
    },
    ReadTextFile {
        req: ReadTextFileRequest,
        reply: Reply<ReadTextFileResponse>,
    },
    WriteTextFile {
        req: WriteTextFileRequest,
        reply: Reply<()>,
    },
    CreateTerminal {
        req: CreateTerminalRequest,
        reply: Reply<CreateTerminalResponse>,
    },
    GetTerminalOutput {
        session_id: SessionId,
        terminal_id: TerminalId,
        reply: Reply<TerminalOutput>,
    },
    WaitForTerminalExit {
        session_id: SessionId,
        terminal_id: TerminalId,
        reply: Reply<TerminalExitStatus>,
    },
    KillTerminal {
        session_id: SessionId,
        terminal_id: TerminalId,
        reply: Reply<()>,
    },
    ReleaseTerminal {
        session_id: SessionId,
        terminal_id: TerminalId,
        reply: Reply<()>,
    },
}

fn provider_only_client<T>(method: &'static str) -> wasmtime::Result<Result<T, Error>> {
    Ok(Err(crate::translate::internal_error(&format!(
        "host bug: routed `client.{method}` to a provider stage"
    ))))
}

// -----------------------------------------------------------------------------
// AccessorTask: dispatched by the actor loop via `accessor.spawn`.
// -----------------------------------------------------------------------------

struct CmdTask {
    bindings: Arc<Bindings>,
    cmd: Cmd,
}

impl AccessorTask<HostState, HasSelf<HostState>> for CmdTask {
    async fn run(self, accessor: &Accessor<HostState>) -> wasmtime::Result<()> {
        let CmdTask { bindings, cmd } = self;
        match cmd {
            // -- agent --
            Cmd::Initialize { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_initialize(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_initialize(accessor, req).await,
                };
                let _ = reply.send(res);
            }
            Cmd::Authenticate { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_authenticate(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_authenticate(accessor, req).await,
                };
                let _ = reply.send(res);
            }
            Cmd::NewSession { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_new_session(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_new_session(accessor, req).await,
                };
                let _ = reply.send(res);
            }
            Cmd::LoadSession { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_load_session(accessor, req).await
                    }
                    Bindings::Layer(b) => b.yosh_acp_agent().call_load_session(accessor, req).await,
                };
                let _ = reply.send(res);
            }
            Cmd::ListSessions { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_list_sessions(accessor, req).await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent().call_list_sessions(accessor, req).await
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::ResumeSession { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_resume_session(accessor, req).await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent().call_resume_session(accessor, req).await
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::SetSessionMode { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent()
                            .call_set_session_mode(accessor, req)
                            .await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent()
                            .call_set_session_mode(accessor, req)
                            .await
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::SelectModel { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_select_model(accessor, req).await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent().call_select_model(accessor, req).await
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::Prompt { req, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => b.yosh_acp_agent().call_prompt(accessor, req).await,
                    Bindings::Layer(b) => b.yosh_acp_agent().call_prompt(accessor, req).await,
                };
                let _ = reply.send(res);
            }
            Cmd::CloseSession { session_id, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent()
                            .call_close_session(accessor, session_id)
                            .await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent()
                            .call_close_session(accessor, session_id)
                            .await
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::Cancel { session_id, reply } => {
                let res = match &*bindings {
                    Bindings::Provider(b) => {
                        b.yosh_acp_agent().call_cancel(accessor, session_id).await
                    }
                    Bindings::Layer(b) => {
                        b.yosh_acp_agent().call_cancel(accessor, session_id).await
                    }
                };
                let _ = reply.send(res);
            }
            // -- client (Layer only) --
            Cmd::UpdateSession {
                session_id,
                update,
                reply,
            } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_update_session(accessor, session_id, update)
                            .await
                    }
                    Bindings::Provider(_) => Err(wasmtime::Error::msg(
                        "host bug: routed `client.update-session` to a provider stage",
                    )),
                };
                let _ = reply.send(res);
            }
            Cmd::RequestPermission { req, reply } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_request_permission(accessor, req)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("request-permission"),
                };
                let _ = reply.send(res);
            }
            Cmd::ReadTextFile { req, reply } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client().call_read_text_file(accessor, req).await
                    }
                    Bindings::Provider(_) => provider_only_client("read-text-file"),
                };
                let _ = reply.send(res);
            }
            Cmd::WriteTextFile { req, reply } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_write_text_file(accessor, req)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("write-text-file"),
                };
                let _ = reply.send(res);
            }
            Cmd::CreateTerminal { req, reply } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_create_terminal(accessor, req)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("create-terminal"),
                };
                let _ = reply.send(res);
            }
            Cmd::GetTerminalOutput {
                session_id,
                terminal_id,
                reply,
            } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_get_terminal_output(accessor, session_id, terminal_id)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("get-terminal-output"),
                };
                let _ = reply.send(res);
            }
            Cmd::WaitForTerminalExit {
                session_id,
                terminal_id,
                reply,
            } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_wait_for_terminal_exit(accessor, session_id, terminal_id)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("wait-for-terminal-exit"),
                };
                let _ = reply.send(res);
            }
            Cmd::KillTerminal {
                session_id,
                terminal_id,
                reply,
            } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_kill_terminal(accessor, session_id, terminal_id)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("kill-terminal"),
                };
                let _ = reply.send(res);
            }
            Cmd::ReleaseTerminal {
                session_id,
                terminal_id,
                reply,
            } => {
                let res = match &*bindings {
                    Bindings::Layer(b) => {
                        b.yosh_acp_client()
                            .call_release_terminal(accessor, session_id, terminal_id)
                            .await
                    }
                    Bindings::Provider(_) => provider_only_client("release-terminal"),
                };
                let _ = reply.send(res);
            }
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// WasmActor — the channel-sender handle.
// -----------------------------------------------------------------------------

/// Channel error: the actor task is gone (graceful shutdown or panic).
#[derive(Debug)]
pub struct ActorGone;

impl std::fmt::Display for ActorGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wasm actor is gone")
    }
}

impl std::error::Error for ActorGone {}

#[derive(Clone)]
pub struct WasmActor {
    tx: mpsc::Sender<Cmd>,
}

impl WasmActor {
    /// Allocate a fresh sender/receiver pair. The sender is the public
    /// handle that gets installed into neighbouring stages' `HostState`
    /// before the actor task is spawned; the receiver is consumed by
    /// [`Self::spawn_loop`].
    pub fn channel() -> (Self, mpsc::Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel::<Cmd>(32);
        (Self { tx }, rx)
    }

    /// Drive the persistent `Store::run_concurrent` event loop on a
    /// blocking task. The loop receives [`Cmd`]s from `rx` and
    /// dispatches each via `accessor.spawn`, so calls execute
    /// concurrently inside one store. Returns when all senders are
    /// dropped (channel closed).
    pub fn spawn_loop(
        mut store: Store<HostState>,
        bindings: Bindings,
        mut rx: mpsc::Receiver<Cmd>,
    ) -> tokio::task::JoinHandle<wasmtime::Result<()>> {
        let bindings = Arc::new(bindings);
        tokio::task::spawn(async move {
            store
                .run_concurrent(async move |accessor| -> wasmtime::Result<()> {
                    while let Some(cmd) = rx.recv().await {
                        accessor.spawn(CmdTask {
                            bindings: bindings.clone(),
                            cmd,
                        });
                    }
                    Ok(())
                })
                .await?
        })
    }

    pub fn downgrade(&self) -> WasmActorWeak {
        WasmActorWeak {
            tx: self.tx.downgrade(),
        }
    }

    async fn send(&self, cmd: Cmd) -> Result<(), ActorGone> {
        self.tx.send(cmd).await.map_err(|_| ActorGone)
    }
}

/// Weak handle to a [`WasmActor`]; used as the `Upstream` back-edge so it
/// doesn't keep a paired downstream's actor alive.
#[derive(Clone)]
pub struct WasmActorWeak {
    tx: mpsc::WeakSender<Cmd>,
}

impl WasmActorWeak {
    pub fn upgrade(&self) -> Option<WasmActor> {
        self.tx.upgrade().map(|tx| WasmActor { tx })
    }
}

// -----------------------------------------------------------------------------
// Typed call_* helpers — match the old WasmAgent API.
// -----------------------------------------------------------------------------

macro_rules! make_call {
    ($name:ident, $variant:ident, $req_ty:ty, $resp_ty:ty) => {
        pub async fn $name(&self, req: $req_ty) -> wasmtime::Result<Result<$resp_ty, Error>> {
            let (tx, rx) = oneshot::channel();
            self.send(Cmd::$variant { req, reply: tx })
                .await
                .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            rx.await
                .map_err(|_| wasmtime::Error::msg("wasm actor dropped reply"))?
        }
    };
}

macro_rules! make_call_session {
    ($name:ident, $variant:ident, $resp_ty:ty) => {
        pub async fn $name(
            &self,
            session_id: SessionId,
        ) -> wasmtime::Result<Result<$resp_ty, Error>> {
            let (tx, rx) = oneshot::channel();
            self.send(Cmd::$variant {
                session_id,
                reply: tx,
            })
            .await
            .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            rx.await
                .map_err(|_| wasmtime::Error::msg("wasm actor dropped reply"))?
        }
    };
}

macro_rules! make_call_terminal {
    ($name:ident, $variant:ident, $resp_ty:ty) => {
        pub async fn $name(
            &self,
            session_id: SessionId,
            terminal_id: TerminalId,
        ) -> wasmtime::Result<Result<$resp_ty, Error>> {
            let (tx, rx) = oneshot::channel();
            self.send(Cmd::$variant {
                session_id,
                terminal_id,
                reply: tx,
            })
            .await
            .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            rx.await
                .map_err(|_| wasmtime::Error::msg("wasm actor dropped reply"))?
        }
    };
}

impl WasmActor {
    make_call!(
        call_initialize,
        Initialize,
        InitializeRequest,
        InitializeResponse
    );
    make_call!(call_authenticate, Authenticate, AuthenticateRequest, ());
    make_call!(
        call_new_session,
        NewSession,
        NewSessionRequest,
        NewSessionResponse
    );
    make_call!(
        call_load_session,
        LoadSession,
        LoadSessionRequest,
        LoadSessionResponse
    );
    make_call!(
        call_list_sessions,
        ListSessions,
        ListSessionsRequest,
        ListSessionsResponse
    );
    make_call!(
        call_resume_session,
        ResumeSession,
        ResumeSessionRequest,
        ResumeSessionResponse
    );
    make_call!(
        call_set_session_mode,
        SetSessionMode,
        SetSessionModeRequest,
        ()
    );
    make_call!(
        call_select_model,
        SelectModel,
        SelectModelRequest,
        ()
    );
    make_call!(call_prompt, Prompt, PromptRequest, PromptResponse);
    make_call_session!(call_close_session, CloseSession, ());

    pub async fn call_cancel(&self, session_id: SessionId) -> wasmtime::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Cancel {
            session_id,
            reply: tx,
        })
        .await
        .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
        rx.await
            .map_err(|_| wasmtime::Error::msg("wasm actor dropped reply"))?
    }

    // Client direction
    pub async fn call_update_session(
        &self,
        session_id: SessionId,
        update: SessionUpdate,
    ) -> wasmtime::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::UpdateSession {
            session_id,
            update,
            reply: tx,
        })
        .await
        .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
        rx.await
            .map_err(|_| wasmtime::Error::msg("wasm actor dropped reply"))?
    }

    make_call!(
        call_request_permission,
        RequestPermission,
        RequestPermissionRequest,
        RequestPermissionResponse
    );
    make_call!(
        call_read_text_file,
        ReadTextFile,
        ReadTextFileRequest,
        ReadTextFileResponse
    );
    make_call!(
        call_write_text_file,
        WriteTextFile,
        WriteTextFileRequest,
        ()
    );
    make_call!(
        call_create_terminal,
        CreateTerminal,
        CreateTerminalRequest,
        CreateTerminalResponse
    );
    make_call_terminal!(call_get_terminal_output, GetTerminalOutput, TerminalOutput);
    make_call_terminal!(
        call_wait_for_terminal_exit,
        WaitForTerminalExit,
        TerminalExitStatus
    );
    make_call_terminal!(call_kill_terminal, KillTerminal, ());
    make_call_terminal!(call_release_terminal, ReleaseTerminal, ());
}
