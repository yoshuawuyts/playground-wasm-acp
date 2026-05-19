//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports).
//!
//! Routing per stage:
//! - Topmost stage's `sink == Outbound`: events go to the bridge task via
//!   the [`OutboundEvent`] mpsc (single owner of stdio).
//! - Inner stages' `sink == Upstream(parent_idx)`: invoke the parent
//!   stage's exported `client` interface on the *same* store. The parent
//!   instance is already executing (its `agent` export is what triggered
//!   this whole call chain), so directly awaiting `bindings.call_*()`
//!   would trap with "cannot enter component instance". We dispatch via
//!   [`Accessor::spawn`] (subtask) + oneshot instead — wasmtime allows
//!   multiple concurrent subtasks per instance under the stackful
//!   concurrent component model.
//!
//! The bindgen-generated `add_to_linker` takes a `fn(&mut T) -> D::Data<'_>`
//! (no captures), so per-stage routing reads [`HostState::current_stage`]
//! from a stack pushed/popped around each `bindings.call_*()` invocation.

use agent_client_protocol::Error as AcpError;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, AccessorTask, HasSelf};

use crate::state::{Bindings, ClientSink, HostState, OutboundEvent};
use crate::translate;
use crate::yosh::acp::client;
use crate::yosh::acp::errors::Error;
use crate::yosh::acp::filesystem::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
};
use crate::yosh::acp::prompts::SessionUpdate;
use crate::yosh::acp::sessions::SessionId;
use crate::yosh::acp::terminals::{
    CreateTerminalRequest, CreateTerminalResponse, TerminalExitStatus, TerminalId, TerminalOutput,
};
use crate::yosh::acp::tools::{RequestPermissionRequest, RequestPermissionResponse};

const OUTBOUND_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn send_and_await<T>(
    outbound: &mpsc::Sender<OutboundEvent>,
    make_event: impl FnOnce(oneshot::Sender<Result<T, AcpError>>) -> OutboundEvent,
    context: &'static str,
) -> Result<T, Error> {
    let (tx, rx) = oneshot::channel();
    outbound
        .send(make_event(tx))
        .await
        .map_err(|_| translate::internal_error(&format!("{context}: bridge task gone")))?;
    match tokio::time::timeout(OUTBOUND_REQUEST_TIMEOUT, rx).await {
        Ok(Ok(Ok(resp))) => Ok(resp),
        Ok(Ok(Err(acp_err))) => Err(translate::acp_error_to_wit(acp_err)),
        Ok(Err(_)) => Err(translate::internal_error(&format!(
            "{context}: bridge dropped reply"
        ))),
        Err(_) => Err(translate::internal_error(&format!(
            "{context}: editor did not respond within {}s",
            OUTBOUND_REQUEST_TIMEOUT.as_secs()
        ))),
    }
}

/// Snapshot of the upstream-routing context taken inside `accessor.with`.
enum Routing {
    Outbound(mpsc::Sender<OutboundEvent>),
    Upstream { idx: usize, bindings: Arc<Bindings> },
}

fn routing(accessor: &Accessor<impl Send, HasSelf<HostState>>) -> Routing {
    accessor.with(|mut a| {
        let state = a.get();
        let stage = state.current_stage();
        match &stage.sink {
            ClientSink::Outbound(tx) => Routing::Outbound(tx.clone()),
            ClientSink::Upstream(idx) => {
                let idx = *idx;
                let bindings = state.stages[idx]
                    .bindings
                    .clone()
                    .expect("upstream stage bindings filled");
                Routing::Upstream { idx, bindings }
            }
        }
    })
}

fn flatten_trap<T>(
    method: &'static str,
    res: wasmtime::Result<Result<T, Error>>,
) -> Result<T, Error> {
    match res {
        Ok(inner) => inner,
        Err(trap) => Err(translate::internal_error(&format!(
            "upstream `client.{method}` trapped: {trap:#}"
        ))),
    }
}

/// Build, spawn and await an upstream re-entrant call as a wasmtime
/// subtask. `make_task` produces the [`AccessorTask`] from a oneshot
/// sender that the task uses to report its result; this function pushes
/// the upstream stage idx, spawns, awaits the reply (with a fallback
/// trap message if the task is dropped), and pops the stage idx.
async fn spawn_upstream<R, Tk, T, F>(
    accessor: &Accessor<T, HasSelf<HostState>>,
    idx: usize,
    method: &'static str,
    make_task: F,
) -> wasmtime::Result<R>
where
    R: Send + 'static,
    T: Send + 'static,
    Tk: AccessorTask<T, HasSelf<HostState>>,
    F: FnOnce(oneshot::Sender<wasmtime::Result<R>>) -> Tk,
{
    accessor.with(|mut a| a.get().push_stage(idx));
    let (tx, rx) = oneshot::channel();
    accessor.spawn(make_task(tx));
    let res = match rx.await {
        Ok(r) => r,
        Err(_) => Err(wasmtime::Error::msg(format!(
            "upstream `client.{method}` task dropped before replying"
        ))),
    };
    accessor.with(|mut a| a.get().pop_stage());
    res
}

// -----------------------------------------------------------------------------
// AccessorTask per upstream client method.
//
// Each one runs `b.yosh_acp_client().call_X(accessor, req).await` and
// forwards the result through the oneshot. The accessor here is the
// *same* one feeding the upstream's host imports, so nested calls (e.g.
// the upstream layer's `client.update-session` impl re-entering an even
// further upstream stage) work transparently.
// -----------------------------------------------------------------------------

struct UpdateSessionTask<T> {
    bindings: Arc<Bindings>,
    session_id: SessionId,
    update: SessionUpdate,
    reply: oneshot::Sender<wasmtime::Result<()>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for UpdateSessionTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_update_session(accessor, self.session_id, self.update)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.update-session",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct RequestPermissionTask<T> {
    bindings: Arc<Bindings>,
    req: RequestPermissionRequest,
    reply: oneshot::Sender<wasmtime::Result<Result<RequestPermissionResponse, Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for RequestPermissionTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_request_permission(accessor, self.req)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.request-permission",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct ReadTextFileTask<T> {
    bindings: Arc<Bindings>,
    req: ReadTextFileRequest,
    reply: oneshot::Sender<wasmtime::Result<Result<ReadTextFileResponse, Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for ReadTextFileTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_read_text_file(accessor, self.req)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.read-text-file",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct WriteTextFileTask<T> {
    bindings: Arc<Bindings>,
    req: WriteTextFileRequest,
    reply: oneshot::Sender<wasmtime::Result<Result<(), Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for WriteTextFileTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_write_text_file(accessor, self.req)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.write-text-file",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct CreateTerminalTask<T> {
    bindings: Arc<Bindings>,
    req: CreateTerminalRequest,
    reply: oneshot::Sender<wasmtime::Result<Result<CreateTerminalResponse, Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for CreateTerminalTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_create_terminal(accessor, self.req)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.create-terminal",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct GetTerminalOutputTask<T> {
    bindings: Arc<Bindings>,
    session_id: SessionId,
    terminal_id: TerminalId,
    reply: oneshot::Sender<wasmtime::Result<Result<TerminalOutput, Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for GetTerminalOutputTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_get_terminal_output(accessor, self.session_id, self.terminal_id)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.get-terminal-output",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct WaitForTerminalExitTask<T> {
    bindings: Arc<Bindings>,
    session_id: SessionId,
    terminal_id: TerminalId,
    reply: oneshot::Sender<wasmtime::Result<Result<TerminalExitStatus, Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for WaitForTerminalExitTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_wait_for_terminal_exit(accessor, self.session_id, self.terminal_id)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.wait-for-terminal-exit",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct KillTerminalTask<T> {
    bindings: Arc<Bindings>,
    session_id: SessionId,
    terminal_id: TerminalId,
    reply: oneshot::Sender<wasmtime::Result<Result<(), Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for KillTerminalTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_kill_terminal(accessor, self.session_id, self.terminal_id)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.kill-terminal",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

struct ReleaseTerminalTask<T> {
    bindings: Arc<Bindings>,
    session_id: SessionId,
    terminal_id: TerminalId,
    reply: oneshot::Sender<wasmtime::Result<Result<(), Error>>>,
    _t: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> AccessorTask<T, HasSelf<HostState>> for ReleaseTerminalTask<T> {
    async fn run(self, accessor: &Accessor<T, HasSelf<HostState>>) -> wasmtime::Result<()> {
        let res = match &*self.bindings {
            Bindings::Layer(b) => {
                b.yosh_acp_client()
                    .call_release_terminal(accessor, self.session_id, self.terminal_id)
                    .await
            }
            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                "host bug: provider stage routed as upstream client.release-terminal",
            )),
        };
        let _ = self.reply.send(res);
        Ok(())
    }
}

impl client::Host for HostState {}

impl client::HostWithStore for HasSelf<HostState> {
    fn update_session<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        update: SessionUpdate,
    ) -> impl ::core::future::Future<Output = ()> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(outbound) => {
                    let Some(notif) = translate::session_update_wit_to_schema(session_id, update)
                    else {
                        return;
                    };
                    let (ack_tx, ack_rx) = oneshot::channel();
                    if outbound
                        .send(OutboundEvent::SessionUpdate(notif, ack_tx))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = ack_rx.await;
                }
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "update-session", |reply| {
                        UpdateSessionTask {
                            bindings,
                            session_id,
                            update,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    if let Err(trap) = res {
                        tracing::warn!(error = %trap, "upstream `client.update-session` trapped");
                    }
                }
            }
        }
    }

    fn request_permission<T: Send>(
        accessor: &Accessor<T, Self>,
        req: RequestPermissionRequest,
    ) -> impl ::core::future::Future<Output = Result<RequestPermissionResponse, Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => {
                    Err(translate::method_not_found("request-permission not wired"))
                }
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "request-permission", |reply| {
                        RequestPermissionTask {
                            bindings,
                            req,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    match res {
                        Ok(Ok(r)) => Ok(r),
                        Ok(Err(wit_err)) => Err(wit_err),
                        Err(trap) => Err(translate::internal_error(&format!(
                            "upstream `client.request-permission` trapped: {trap:#}"
                        ))),
                    }
                }
            }
        }
    }

    fn read_text_file<T: Send>(
        accessor: &Accessor<T, Self>,
        req: ReadTextFileRequest,
    ) -> impl ::core::future::Future<Output = Result<ReadTextFileResponse, Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(outbound) => {
                    let schema_req = translate::read_text_file_request_wit_to_schema(req);
                    let resp = send_and_await(
                        &outbound,
                        |tx| OutboundEvent::ReadTextFile(schema_req, tx),
                        "fs/read",
                    )
                    .await?;
                    Ok(translate::read_text_file_response_schema_to_wit(resp))
                }
                Routing::Upstream { idx, bindings } => {
                    let res =
                        spawn_upstream(accessor, idx, "read-text-file", |reply| ReadTextFileTask {
                            bindings,
                            req,
                            reply,
                            _t: PhantomData,
                        })
                        .await;
                    flatten_trap("read-text-file", res)
                }
            }
        }
    }

    fn write_text_file<T: Send>(
        accessor: &Accessor<T, Self>,
        req: WriteTextFileRequest,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(outbound) => {
                    let schema_req = translate::write_text_file_request_wit_to_schema(req);
                    send_and_await(
                        &outbound,
                        |tx| OutboundEvent::WriteTextFile(schema_req, tx),
                        "fs/write",
                    )
                    .await?;
                    Ok(())
                }
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "write-text-file", |reply| {
                        WriteTextFileTask {
                            bindings,
                            req,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    flatten_trap("write-text-file", res)
                }
            }
        }
    }

    fn create_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        req: CreateTerminalRequest,
    ) -> impl ::core::future::Future<Output = Result<CreateTerminalResponse, Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => {
                    Err(translate::method_not_found("create-terminal not wired"))
                }
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "create-terminal", |reply| {
                        CreateTerminalTask {
                            bindings,
                            req,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    flatten_trap("create-terminal", res)
                }
            }
        }
    }

    fn get_terminal_output<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<TerminalOutput, Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => Err(translate::method_not_found(
                    "get-terminal-output not supported",
                )),
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "get-terminal-output", |reply| {
                        GetTerminalOutputTask {
                            bindings,
                            session_id,
                            terminal_id,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    flatten_trap("get-terminal-output", res)
                }
            }
        }
    }

    fn wait_for_terminal_exit<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<TerminalExitStatus, Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => Err(translate::method_not_found(
                    "wait-for-terminal-exit not supported",
                )),
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "wait-for-terminal-exit", |reply| {
                        WaitForTerminalExitTask {
                            bindings,
                            session_id,
                            terminal_id,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    flatten_trap("wait-for-terminal-exit", res)
                }
            }
        }
    }

    fn kill_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => {
                    Err(translate::method_not_found("kill-terminal not supported"))
                }
                Routing::Upstream { idx, bindings } => {
                    let res =
                        spawn_upstream(accessor, idx, "kill-terminal", |reply| KillTerminalTask {
                            bindings,
                            session_id,
                            terminal_id,
                            reply,
                            _t: PhantomData,
                        })
                        .await;
                    flatten_trap("kill-terminal", res)
                }
            }
        }
    }

    fn release_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let route = routing(accessor);
        async move {
            match route {
                Routing::Outbound(_) => Err(translate::method_not_found(
                    "release-terminal not supported",
                )),
                Routing::Upstream { idx, bindings } => {
                    let res = spawn_upstream(accessor, idx, "release-terminal", |reply| {
                        ReleaseTerminalTask {
                            bindings,
                            session_id,
                            terminal_id,
                            reply,
                            _t: PhantomData,
                        }
                    })
                    .await;
                    flatten_trap("release-terminal", res)
                }
            }
        }
    }
}
