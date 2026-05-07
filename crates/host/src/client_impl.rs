//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports). With the WIT now using async functions, the bindgen output puts
//! method bodies on the `HostWithStore` trait (static methods taking an
//! `Accessor`); the original `Host` trait is just a `Send` marker.

use agent_client_protocol::Error as AcpError;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, HasSelf};

use crate::state::{ClientSink, HostState, OutboundEvent};
use crate::translate;
use crate::wasm::{Bindings, WasmAgent};
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

fn trap_to_wit<T>(method: &'static str, res: wasmtime::Result<wasmtime::Result<Result<T, Error>>>) -> Result<T, Error> {
    match res {
        Ok(Ok(inner)) => inner,
        Ok(Err(trap)) | Err(trap) => Err(translate::internal_error(&format!(
            "upstream `{method}` trapped: {trap:#}"
        ))),
    }
}

impl client::Host for HostState {}

// -----------------------------------------------------------------------------
// client::HostWithStore — routes outbound client calls upward.
// -----------------------------------------------------------------------------
//
// Same recursion-guard concern as the agent direction (see `wasm.rs`):
// upstream calls are spawned onto a fresh tokio task so wasmtime's
// per-task `run_concurrent` recursion check doesn't trip.

macro_rules! upstream_call {
    ($method:literal, $accessor:ident, $req:ident, $call:ident) => {{
        let sink = $accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => Err(translate::method_not_found(concat!(
                    $method,
                    " not wired"
                ))),
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(concat!(
                            "upstream `client.",
                            $method,
                            "` gone"
                        )));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| client.$call(a, $req).await)
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(concat!(
                                "host bug: routed `client.",
                                $method,
                                "` to a provider stage"
                            ))))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit($method, res)
                }
            }
        }
    }};
}

impl client::HostWithStore for HasSelf<HostState> {
    fn update_session<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        update: SessionUpdate,
    ) -> impl ::core::future::Future<Output = ()> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(outbound) => {
                    let Some(notif) = translate::session_update_wit_to_schema(session_id, update)
                    else {
                        return;
                    };
                    let _ = outbound.send(OutboundEvent::SessionUpdate(notif)).await;
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        tracing::warn!("upstream `client.update-session` gone");
                        return;
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client.call_update_session(a, session_id, update).await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                "host bug: routed `client.update-session` to a provider stage",
                            )),
                        }
                    })
                    .await;
                    if let Err(e) = join {
                        tracing::warn!(error = %e, "upstream `client.update-session` task join error");
                    } else if let Ok(Err(trap)) = join {
                        tracing::warn!(error = %trap, "upstream `client.update-session` trapped");
                    } else if let Ok(Ok(Err(trap))) = join {
                        tracing::warn!(error = %trap, "upstream `client.update-session` trapped");
                    }
                }
            }
        }
    }

    fn request_permission<T: Send>(
        accessor: &Accessor<T, Self>,
        req: RequestPermissionRequest,
    ) -> impl ::core::future::Future<Output = Result<RequestPermissionResponse, Error>> + Send
    {
        upstream_call!("request-permission", accessor, req, call_request_permission)
    }

    fn read_text_file<T: Send>(
        accessor: &Accessor<T, Self>,
        req: ReadTextFileRequest,
    ) -> impl ::core::future::Future<Output = Result<ReadTextFileResponse, Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(outbound) => {
                    let schema_req = translate::read_text_file_request_wit_to_schema(req);
                    let resp = send_and_await(
                        &outbound,
                        |tx| OutboundEvent::ReadTextFile(schema_req, tx),
                        "fs/read",
                    )
                    .await?;
                    Ok(translate::read_text_file_response_schema_to_wit(resp))
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.read-text-file` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client.call_read_text_file(a, req).await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.read-text-file` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("read-text-file", res)
                }
            }
        }
    }

    fn write_text_file<T: Send>(
        accessor: &Accessor<T, Self>,
        req: WriteTextFileRequest,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(outbound) => {
                    let schema_req = translate::write_text_file_request_wit_to_schema(req);
                    send_and_await(
                        &outbound,
                        |tx| OutboundEvent::WriteTextFile(schema_req, tx),
                        "fs/write",
                    )
                    .await?;
                    Ok(())
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.write-text-file` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client.call_write_text_file(a, req).await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.write-text-file` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("write-text-file", res)
                }
            }
        }
    }

    fn create_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        req: CreateTerminalRequest,
    ) -> impl ::core::future::Future<Output = Result<CreateTerminalResponse, Error>> + Send {
        upstream_call!("create-terminal", accessor, req, call_create_terminal)
    }

    fn get_terminal_output<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<TerminalOutput, Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => Err(translate::method_not_found(
                    "get-terminal-output not supported",
                )),
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.get-terminal-output` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client
                                            .call_get_terminal_output(a, session_id, terminal_id)
                                            .await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.get-terminal-output` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("get-terminal-output", res)
                }
            }
        }
    }

    fn wait_for_terminal_exit<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<TerminalExitStatus, Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => Err(translate::method_not_found(
                    "wait-for-terminal-exit not supported",
                )),
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.wait-for-terminal-exit` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client
                                            .call_wait_for_terminal_exit(a, session_id, terminal_id)
                                            .await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.wait-for-terminal-exit` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("wait-for-terminal-exit", res)
                }
            }
        }
    }

    fn kill_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => {
                    Err(translate::method_not_found("kill-terminal not supported"))
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.kill-terminal` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client.call_kill_terminal(a, session_id, terminal_id).await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.kill-terminal` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("kill-terminal", res)
                }
            }
        }
    }

    fn release_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        session_id: SessionId,
        terminal_id: TerminalId,
    ) -> impl ::core::future::Future<Output = Result<(), Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => Err(translate::method_not_found(
                    "release-terminal not supported",
                )),
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return Err(translate::internal_error(
                            "upstream `client.release-terminal` gone",
                        ));
                    };
                    let join = tokio::task::spawn(async move {
                        let mut guard = upstream.lock().await;
                        let WasmAgent { store, bindings } = &mut *guard;
                        match bindings {
                            Bindings::Layer(b) => {
                                let client = b.yosh_acp_client();
                                store
                                    .run_concurrent(async move |a| {
                                        client
                                            .call_release_terminal(a, session_id, terminal_id)
                                            .await
                                    })
                                    .await
                            }
                            Bindings::Provider(_) => Ok(Ok(Err(translate::internal_error(
                                "host bug: routed `client.release-terminal` to a provider stage",
                            )))),
                        }
                    })
                    .await;
                    let res = match join {
                        Ok(r) => r,
                        Err(e) => Err(wasmtime::Error::msg(format!(
                            "upstream task join error: {e}"
                        ))),
                    };
                    trap_to_wit("release-terminal", res)
                }
            }
        }
    }
}
