//! Implementation of the ACP `client` interface (the methods the wasm guest
//! imports). With the WIT now using async functions, the bindgen output puts
//! method bodies on the `HostWithStore` trait (static methods taking an
//! `Accessor`); the original `Host` trait is just a `Send` marker.
//!
//! Routing: the topmost stage's `client_sink` is `Outbound`, sending events
//! to the bridge task; intermediate stages route into the upstream layer's
//! exported `client` interface via the per-stage [`crate::wasm_actor::WasmActor`].
//! Sending a [`crate::wasm_actor::Cmd`] on the upstream actor's channel
//! avoids any cross-store mutex or nested `run_concurrent`.

use agent_client_protocol::Error as AcpError;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Accessor, HasSelf};

use crate::state::{ClientSink, HostState, OutboundEvent};
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

fn upstream_gone<T>(method: &'static str) -> Result<T, Error> {
    Err(translate::internal_error(&format!(
        "upstream `client.{method}` gone"
    )))
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
                    if let Err(trap) = upstream.call_update_session(session_id, update).await {
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
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => {
                    Err(translate::method_not_found("request-permission not wired"))
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("request-permission");
                    };
                    flatten_trap(
                        "request-permission",
                        upstream.call_request_permission(req).await,
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
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("read-text-file");
                    };
                    flatten_trap("read-text-file", upstream.call_read_text_file(req).await)
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
                        return upstream_gone("write-text-file");
                    };
                    flatten_trap("write-text-file", upstream.call_write_text_file(req).await)
                }
            }
        }
    }

    fn create_terminal<T: Send>(
        accessor: &Accessor<T, Self>,
        req: CreateTerminalRequest,
    ) -> impl ::core::future::Future<Output = Result<CreateTerminalResponse, Error>> + Send {
        let sink = accessor.with(|mut a| a.get().client_sink.clone());
        async move {
            match sink {
                ClientSink::Outbound(_) => {
                    Err(translate::method_not_found("create-terminal not wired"))
                }
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("create-terminal");
                    };
                    flatten_trap("create-terminal", upstream.call_create_terminal(req).await)
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
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("get-terminal-output");
                    };
                    flatten_trap(
                        "get-terminal-output",
                        upstream
                            .call_get_terminal_output(session_id, terminal_id)
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
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("wait-for-terminal-exit");
                    };
                    flatten_trap(
                        "wait-for-terminal-exit",
                        upstream
                            .call_wait_for_terminal_exit(session_id, terminal_id)
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
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("kill-terminal");
                    };
                    flatten_trap(
                        "kill-terminal",
                        upstream.call_kill_terminal(session_id, terminal_id).await,
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
                ClientSink::Upstream(weak) => {
                    let Some(upstream) = weak.upgrade() else {
                        return upstream_gone("release-terminal");
                    };
                    flatten_trap(
                        "release-terminal",
                        upstream
                            .call_release_terminal(session_id, terminal_id)
                            .await,
                    )
                }
            }
        }
    }
}
