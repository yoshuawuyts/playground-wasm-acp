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
                    let Some(notif) =
                        translate::session_update_wit_to_schema(session_id, update)
                    else {
                        return;
                    };
                    let _ = outbound.send(OutboundEvent::SessionUpdate(notif)).await;
                }
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { tracing::warn!("upstream `client.update-session` gone"); return; };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    let res = store
                        .run_concurrent(async move |a| match bindings_ref {
                            Bindings::Layer(b) => {
                                b.yosh_acp_client()
                                    .call_update_session(a, session_id, update)
                                    .await
                            }
                            Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                "host bug: routed `client.update-session` to a provider stage",
                            )),
                        })
                        .await;
                    if let Err(trap) = res.and_then(|x| x) {
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
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => {
                    Err(translate::method_not_found("request-permission not wired"))
                }
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "request-permission",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client().call_request_permission(a, req).await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.request-permission` to a provider stage",
                                )),
                            })
                            .await,
                    )
                }
            }
        }
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "read-text-file",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client().call_read_text_file(a, req).await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.read-text-file` to a provider stage",
                                )),
                            })
                            .await,
                    )
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "write-text-file",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client().call_write_text_file(a, req).await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.write-text-file` to a provider stage",
                                )),
                            })
                            .await,
                    )
                }
            }
        }
    }

    fn create_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        req: CreateTerminalRequest,
    ) -> impl ::core::future::Future<Output = Result<CreateTerminalResponse, Error>> + Send
    {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => {
                    Err(translate::method_not_found("create-terminal not supported"))
                }
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "create-terminal",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client().call_create_terminal(a, req).await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.create-terminal` to a provider stage",
                                )),
                            })
                            .await,
                    )
                }
            }
        }
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "get-terminal-output",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client()
                                        .call_get_terminal_output(a, session_id, terminal_id)
                                        .await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.get-terminal-output` to a provider stage",
                                )),
                            })
                            .await,
                    )
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "wait-for-terminal-exit",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client()
                                        .call_wait_for_terminal_exit(a, session_id, terminal_id)
                                        .await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.wait-for-terminal-exit` to a provider stage",
                                )),
                            })
                            .await,
                    )
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "kill-terminal",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client()
                                        .call_kill_terminal(a, session_id, terminal_id)
                                        .await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.kill-terminal` to a provider stage",
                                )),
                            })
                            .await,
                    )
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
                ClientSink::Upstream(upstream) => {
                    let Some(upstream) = upstream.upgrade() else { return Err(translate::internal_error("upstream gone")); };
                    let mut guard = upstream.lock().await;
                    let WasmAgent { store, bindings } = &mut *guard;
                    let bindings_ref: &Bindings = bindings;
                    trap_to_wit(
                        "release-terminal",
                        store
                            .run_concurrent(async move |a| match bindings_ref {
                                Bindings::Layer(b) => {
                                    b.yosh_acp_client()
                                        .call_release_terminal(a, session_id, terminal_id)
                                        .await
                                }
                                Bindings::Provider(_) => Err(wasmtime::Error::msg(
                                    "host bug: routed `client.release-terminal` to a provider stage",
                                )),
                            })
                            .await,
                    )
                }
            }
        }
    }
}
