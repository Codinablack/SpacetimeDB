use std::mem;
use std::pin::{pin, Pin};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Extension;
use axum_extra::TypedHeader;
use bytes::Bytes;
use bytestring::ByteString;
use futures::future::MaybeDone;
use futures::{Future, FutureExt, SinkExt, StreamExt};
use http::{HeaderValue, StatusCode};
use scopeguard::ScopeGuard;
use serde::Deserialize;
use spacetimedb::client::messages::{serialize, IdentityTokenMessage, SerializableMessage, SerializeBuffer};
use spacetimedb::client::{
    ClientActorId, ClientConfig, ClientConnection, DataMessage, MessageHandleError, MeteredDeque, MeteredReceiver,
    Protocol,
};
use spacetimedb::execution_context::WorkloadType;
use spacetimedb::host::module_host::ClientConnectedError;
use spacetimedb::host::NoSuchModule;
use spacetimedb::util::also_poll;
use spacetimedb::worker_metrics::WORKER_METRICS;
use spacetimedb::Identity;
use spacetimedb_client_api_messages::websocket::{self as ws_api, Compression};
use spacetimedb_lib::connection_id::{ConnectionId, ConnectionIdForUrl};
use std::time::Instant;
use tokio_tungstenite::tungstenite::Utf8Bytes;

use crate::auth::SpacetimeAuth;
use crate::util::websocket::{
    CloseCode, CloseFrame, Message as WsMessage, WebSocketConfig, WebSocketStream, WebSocketUpgrade,
};
use crate::util::{NameOrIdentity, XForwardedFor};
use crate::{log_and_500, ControlStateDelegate, NodeDelegate};

#[allow(clippy::declare_interior_mutable_const)]
pub const TEXT_PROTOCOL: HeaderValue = HeaderValue::from_static(ws_api::TEXT_PROTOCOL);
#[allow(clippy::declare_interior_mutable_const)]
pub const BIN_PROTOCOL: HeaderValue = HeaderValue::from_static(ws_api::BIN_PROTOCOL);

#[derive(Deserialize)]
pub struct SubscribeParams {
    pub name_or_identity: NameOrIdentity,
}

#[derive(Deserialize)]
pub struct SubscribeQueryParams {
    pub connection_id: Option<ConnectionIdForUrl>,
    #[serde(default)]
    pub compression: Compression,
    /// Whether we want "light" responses, tailored to network bandwidth constrained clients.
    /// This knob works by setting other, more specific, knobs to the value.
    #[serde(default)]
    pub light: bool,
}

pub fn generate_random_connection_id() -> ConnectionId {
    ConnectionId::from_le_byte_array(rand::random())
}

pub async fn handle_websocket<S>(
    State(ctx): State<S>,
    Path(SubscribeParams { name_or_identity }): Path<SubscribeParams>,
    Query(SubscribeQueryParams {
        connection_id,
        compression,
        light,
    }): Query<SubscribeQueryParams>,
    forwarded_for: Option<TypedHeader<XForwardedFor>>,
    Extension(auth): Extension<SpacetimeAuth>,
    ws: WebSocketUpgrade,
) -> axum::response::Result<impl IntoResponse>
where
    S: NodeDelegate + ControlStateDelegate,
{
    if connection_id.is_some() {
        // TODO: Bump this up to `log::warn!` after removing the client SDKs' uses of that parameter.
        log::debug!("The connection_id query parameter to the subscribe HTTP endpoint is internal and will be removed in a future version of SpacetimeDB.");
    }

    let connection_id = connection_id
        .map(ConnectionId::from)
        .unwrap_or_else(generate_random_connection_id);

    if connection_id == ConnectionId::ZERO {
        Err((
            StatusCode::BAD_REQUEST,
            "Invalid connection ID: the all-zeros ConnectionId is reserved.",
        ))?;
    }

    let db_identity = name_or_identity.resolve(&ctx).await?;

    let (res, ws_upgrade, protocol) =
        ws.select_protocol([(BIN_PROTOCOL, Protocol::Binary), (TEXT_PROTOCOL, Protocol::Text)]);

    let protocol = protocol.ok_or((StatusCode::BAD_REQUEST, "no valid protocol selected"))?;
    let client_config = ClientConfig {
        protocol,
        compression,
        tx_update_full: !light,
    };

    // TODO: Should also maybe refactor the code and the protocol to allow a single websocket
    // to connect to multiple modules

    let database = ctx
        .get_database_by_identity(&db_identity)
        .unwrap()
        .ok_or(StatusCode::NOT_FOUND)?;

    let leader = ctx
        .leader(database.id)
        .await
        .map_err(log_and_500)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let identity_token = auth.creds.token().into();

    let module_rx = leader.module_watcher().await.map_err(log_and_500)?;

    let client_id = ClientActorId {
        identity: auth.identity,
        connection_id,
        name: ctx.client_actor_index().next_client_name(),
    };

    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(0x2000000))
        .max_frame_size(None)
        .accept_unmasked_frames(false);

    tokio::spawn(async move {
        let ws = match ws_upgrade.upgrade(ws_config).await {
            Ok(ws) => ws,
            Err(err) => {
                log::error!("WebSocket init error: {}", err);
                return;
            }
        };

        match forwarded_for {
            Some(TypedHeader(XForwardedFor(ip))) => {
                log::debug!("New client connected from ip {}", ip)
            }
            None => log::debug!("New client connected from unknown ip"),
        }

        let actor = |client, sendrx| ws_client_actor(client, ws, sendrx);
        let client = match ClientConnection::spawn(client_id, client_config, leader.replica_id, module_rx, actor).await
        {
            Ok(s) => s,
            Err(e @ (ClientConnectedError::Rejected(_) | ClientConnectedError::OutOfEnergy)) => {
                log::info!("{e}");
                return;
            }
            Err(e @ (ClientConnectedError::DBError(_) | ClientConnectedError::ReducerCall(_))) => {
                log::warn!("ModuleHost died while we were connecting: {e:#}");
                return;
            }
        };

        // Send the client their identity token message as the first message
        // NOTE: We're adding this to the protocol because some client libraries are
        // unable to access the http response headers.
        // Clients that receive the token from the response headers should ignore this
        // message.
        let message = IdentityTokenMessage {
            identity: auth.identity,
            token: identity_token,
            connection_id,
        };
        if let Err(e) = client.send_message(message) {
            log::warn!("{e}, before identity token was sent")
        }
    });

    Ok(res)
}

const LIVELINESS_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

async fn ws_client_actor(client: ClientConnection, ws: WebSocketStream, sendrx: MeteredReceiver<SerializableMessage>) {
    // ensure that even if this task gets cancelled, we always cleanup the connection
    let mut client = scopeguard::guard(client, |client| {
        tokio::spawn(client.disconnect());
    });

    ws_client_actor_inner(&mut client, ws, sendrx).await;

    ScopeGuard::into_inner(client).disconnect().await;
}

async fn make_progress<Fut: Future>(fut: &mut Pin<&mut MaybeDone<Fut>>) {
    if let MaybeDone::Gone = **fut {
        // nothing to do
    } else {
        fut.await
    }
}

async fn ws_client_actor_inner(
    client: &mut ClientConnection,
    mut ws: WebSocketStream,
    mut sendrx: MeteredReceiver<SerializableMessage>,
) {
    let mut liveness_check_interval = tokio::time::interval(LIVELINESS_TIMEOUT);
    let mut got_pong = true;

    let addr = client.module.info().database_identity;

    // Build a queue of incoming messages to handle, to be processed one at a time,
    // in the order they're received.
    //
    // N.B. if you're refactoring this code: you must ensure the handle_queue is dropped before
    // client.disconnect() is called. Otherwise, we can be left with a stale future that's never
    // awaited, which can lead to bugs like:
    // https://rust-lang.github.io/wg-async/vision/submitted_stories/status_quo/aws_engineer/solving_a_deadlock.html
    //
    // NOTE: never let this go unpolled while you're awaiting something; otherwise, it's possible
    //       to deadlock or delay for a long time. see usage of `also_poll()` in the branches of the
    //       `select!` for examples of how to do this.
    //
    // TODO: do we want this to have a fixed capacity? or should it be unbounded
    let mut message_queue = MeteredDeque::<(DataMessage, Instant)>::new(
        WORKER_METRICS.total_incoming_queue_length.with_label_values(&addr),
    );
    let mut current_message = pin!(MaybeDone::Gone);

    let mut closed = false;
    let mut rx_buf = Vec::new();

    let mut msg_buffer = SerializeBuffer::new(client.config);
    loop {
        rx_buf.clear();
        enum Item {
            Message(ClientMessage),
            HandleResult(Result<(), MessageHandleError>),
        }
        if let MaybeDone::Gone = *current_message {
            if let Some((message, timer)) = message_queue.pop_front() {
                let client = client.clone();
                let fut = async move { client.handle_message(message, timer).await };
                current_message.set(MaybeDone::Future(fut));
            }
        }
        let message = tokio::select! {
            // NOTE: all of the futures for these branches **must** be cancel safe. do not
            //       change this if you don't know what that means.

            // If we have a result from handling a past message to report,
            // grab it to handle in the next `match`.
            Some(res) = async {
                make_progress(&mut current_message).await;
                current_message.as_mut().take_output()
            } => {
                Item::HandleResult(res)
            }

            // If we've received an incoming message,
            // grab it to handle in the next `match`.
            message = ws.next() => match message {
                Some(Ok(m)) => Item::Message(ClientMessage::from_message(m)),
                Some(Err(error)) => {
                    log::warn!("Websocket receive error: {}", error);
                    break;
                }
                // the client sent us a close frame
                None => {
                    break;
                }
            },

            // If we have an outgoing message to send, send it off.
            // No incoming `message` to handle, so `continue`.
            Some(n) = sendrx.recv_many(&mut rx_buf, 32).map(|n| (n != 0).then_some(n)) => {
                if closed {
                    // TODO: this isn't great. when we receive a close request from the peer,
                    //       tungstenite doesn't let us send any new messages on the socket,
                    //       even though the websocket RFC allows it. should we fork tungstenite?
                    log::info!("dropping {n} messages due to ws already being closed");
                    log::debug!("dropped messages: {:?}", &rx_buf[..n]);
                } else {
                    let send_all = async {
                        for msg in rx_buf.drain(..n) {
                            let workload = msg.workload();
                            let num_rows = msg.num_rows();

                            // Serialize the message, report metrics,
                            // and keep a handle to the buffer.
                            let (msg_alloc, msg_data) = serialize(msg_buffer, msg, client.config);
                            report_ws_sent_metrics(&addr, workload, num_rows, &msg_data);

                            // Buffer the message without necessarily sending it.
                            let res = ws.feed(datamsg_to_wsmsg(msg_data)).await;

                            // At this point,
                            // the underlying allocation of `msg_data` should have a single referent
                            // and this should be `msg_alloc`.
                            // We can put this back into our pool.
                            msg_buffer = msg_alloc.try_reclaim()
                                .expect("should have a unique referent to `msg_alloc`");

                            if res.is_err() {
                                return (res, msg_buffer);
                            }
                        }
                        // now we flush all the messages to the socket
                        (ws.flush().await, msg_buffer)
                    };
                    // Build a future that both times out and drives the send.
                    //
                    // Note that if flushing cannot immediately complete for whatever reason,
                    // it will wait without polling the other futures in the `select!` arms.
                    // Among other things, this means our liveness tick will not be polled.
                    //
                    // To avoid waiting indefinitely, we wrap the send in a timeout.
                    // A timeout is treated as an unresponsive client and we drop the connection.
                    let send_all = tokio::time::timeout(SEND_TIMEOUT, send_all);
                    // Flush the websocket while continuing to poll the `handle_queue`,
                    // to avoid deadlocks or delays due to enqueued futures holding resources.
                    let send_all = also_poll(send_all, make_progress(&mut current_message));
                    let t1 = Instant::now();
                    let (send_all_result, buf) = match send_all.await {
                        Ok((send_all_result, buf)) => {
                            (send_all_result, buf)
                        }
                        Err(e) => {
                            // Our send timed out; drop client without trying to send them a Close
                            log::warn!("send_all timed out: {e}");
                            break;
                        }
                    };
                    msg_buffer = buf;
                    if let Err(error) = send_all_result {
                        log::warn!("Websocket send error: {error}")
                    }
                    let time = t1.elapsed();
                    if time > Duration::from_millis(50) {
                        tracing::warn!(?time, "send_all took a very long time");
                    }
                }
                continue;
            }

            res = client.watch_module_host(), if !closed => {
                match res {
                    Ok(()) => {}
                    // If the module has exited, close the websocket.
                    Err(NoSuchModule) => {
                        // Send a close frame while continuing to poll the `handle_queue`,
                        // to avoid deadlocks or delays due to enqueued futures holding resources.
                        let close = ws.close(Some(CloseFrame { code: CloseCode::Away, reason: "module exited".into() }));
                        // Wrap the close in a timeout
                        let close = tokio::time::timeout(SEND_TIMEOUT, close);
                        match also_poll(close, make_progress(&mut current_message)).await {
                            Ok(Err(e)) => {
                                log::warn!("error closing websocket: {e:#}")
                            }
                            Err(e) => {
                                // Our send timed out; drop client without trying to send them a Close.
                                //
                                // Is it correct to break if a reducer is still in progress?
                                // Answer: Yes it is.
                                //
                                // If a reducer is currently being executed,
                                // we are waiting for the `current_message` future to complete.
                                // When we break, the task completes and this future is dropped.
                                //
                                // Notably though the reducer itself will run to completion,
                                // however when it tries to notify this task that it is done,
                                // it will encounter a closed sender in `JobThread::run`,
                                // dropping the value that it's trying to send.
                                // In particular it will not throw an error or panic.
                                log::warn!("websocket close timed out: {e}");
                                break;
                            }
                            _ => {}
                        };
                        closed = true;
                    }
                }
                continue;
            }

            // If it's time to send a ping...
            _ = liveness_check_interval.tick() => {
                // If we received a pong at some point, send a fresh ping.
                if mem::take(&mut got_pong) {
                    // Build a future that both times out and drives the send.
                    //
                    // Note that if the send cannot immediately complete for whatever reason,
                    // it will wait without polling the other futures in the `select!` arms.
                    // Among other things, this means we won't poll the websocket for a Close frame.
                    //
                    // To avoid waiting indefinitely, we wrap the ping in a timeout.
                    // A timeout is treated as an unresponsive client and we drop the connection.
                    let ping = ws.send(WsMessage::Ping(Bytes::new()));
                    let ping_with_timeout = tokio::time::timeout(SEND_TIMEOUT, ping);

                    // Send a ping message while continuing to poll the `handle_queue`,
                    // to avoid deadlocks or delays due to enqueued futures holding resources.
                    match also_poll(ping_with_timeout, make_progress(&mut current_message)).await {
                        Ok(Err(e)) => {
                            log::warn!("error sending ping: {e:#}");
                        }
                        Err(e) => {
                            // Our ping timed out; drop them without trying to send them a Close
                            log::warn!("ping timed out after: {e}");
                            break;
                        }
                        _ => {}
                    }
                    continue;
                } else {
                    // the client never responded to our ping; drop them without trying to send them a Close
                    log::warn!("client {} timed out", client.id);
                    break;
                }
            }
        };

        // Handle the incoming message we grabbed in the previous `select!`.

        // TODO: Data flow appears to not require `enum Item` or this distinct `match`,
        //       since `Item::HandleResult` comes from exactly one `select!` branch,
        //       and `Item::Message` comes from exactly one distinct `select!` branch.
        //       Consider merging this `match` with the previous `select!`.
        match message {
            Item::Message(ClientMessage::Message(message)) => {
                let timer = Instant::now();
                message_queue.push_back((message, timer))
            }
            Item::HandleResult(res) => {
                if let Err(e) = res {
                    if let MessageHandleError::Execution(err) = e {
                        log::error!("reducer execution error: {err:#}");
                        // Serialize the message and keep a handle to the buffer.
                        let (msg_alloc, msg_data) = serialize(msg_buffer, err, client.config);

                        let send = async { ws.send(datamsg_to_wsmsg(msg_data)).await };
                        let send = tokio::time::timeout(SEND_TIMEOUT, send);

                        match send.await {
                            Ok(Err(error)) => {
                                log::warn!("Websocket send error: {error}")
                            }
                            Err(error) => {
                                log::warn!("send timed out after: {error}");
                                break;
                            }
                            _ => {}
                        }

                        // At this point,
                        // the underlying allocation of `msg_data` should have a single referent
                        // and this should be `msg_alloc`.
                        // We can put this back into our pool.
                        msg_buffer = msg_alloc
                            .try_reclaim()
                            .expect("should have a unique referent to `msg_alloc`");

                        continue;
                    }
                    log::warn!("Client caused error on text message: {}", e);
                    let close = ws.close(Some(CloseFrame {
                        code: CloseCode::Error,
                        reason: format!("{e:#}").into(),
                    }));

                    // Wrap the close in a timeout
                    match tokio::time::timeout(SEND_TIMEOUT, close).await {
                        Ok(Err(e)) => {
                            log::warn!("error closing websocket: {e:#}")
                        }
                        Err(e) => {
                            log::warn!("send timed out after: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Item::Message(ClientMessage::Ping(_message)) => {
                log::trace!("Received ping from client {}", client.id);
                // No need to explicitly respond with a `Pong`, as tungstenite handles this automatically.
                // See [https://github.com/snapview/tokio-tungstenite/issues/88].
            }
            Item::Message(ClientMessage::Pong(_message)) => {
                log::trace!("Received heartbeat from client {}", client.id);
                got_pong = true;
            }
            Item::Message(ClientMessage::Close(close_frame)) => {
                // This happens in 2 cases:
                // a) We sent a Close frame and this is the ack.
                // b) This is the client telling us they want to close.
                // in either case, after the remaining messages in the queue flush,
                // ws.next() will return None and we'll exit the loop.
                // NOTE: No need to send a close frame, it's is queued
                //       automatically by tungstenite.

                // if this is the closed-by-them case, let the ClientConnectionSenders know now.
                sendrx.close();
                log::trace!("Close frame {:?}", close_frame);
                if !closed {
                    // This is the client telling us they want to close.
                    WORKER_METRICS
                        .ws_clients_closed_connection
                        .with_label_values(&addr)
                        .inc();
                }

                // Can't we just break out of the loop here?
                // Not, if we want tungstenite to send a close frame back to the client.
                // That will only happen once `ws.next()` returns `None`.
                closed = true;
            }
        }
    }
    log::debug!("Client connection ended");
    sendrx.close();
}

enum ClientMessage {
    Message(DataMessage),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<CloseFrame>),
}
impl ClientMessage {
    fn from_message(msg: WsMessage) -> Self {
        match msg {
            WsMessage::Text(s) => Self::Message(DataMessage::Text(utf8bytes_to_bytestring(s))),
            WsMessage::Binary(b) => Self::Message(DataMessage::Binary(b)),
            WsMessage::Ping(b) => Self::Ping(b),
            WsMessage::Pong(b) => Self::Pong(b),
            WsMessage::Close(frame) => Self::Close(frame),
            // WebSocket::read_message() never returns a raw Message::Frame
            WsMessage::Frame(_) => unreachable!(),
        }
    }
}

/// Report metrics on sent rows and message sizes to a websocket client.
fn report_ws_sent_metrics(
    addr: &Identity,
    workload: Option<WorkloadType>,
    num_rows: Option<usize>,
    msg_ws: &DataMessage,
) {
    // These metrics should be updated together,
    // or not at all.
    if let (Some(workload), Some(num_rows)) = (workload, num_rows) {
        WORKER_METRICS
            .websocket_sent_num_rows
            .with_label_values(addr, &workload)
            .observe(num_rows as f64);
        WORKER_METRICS
            .websocket_sent_msg_size
            .with_label_values(addr, &workload)
            .observe(msg_ws.len() as f64);
    }
}

fn datamsg_to_wsmsg(msg: DataMessage) -> WsMessage {
    match msg {
        DataMessage::Text(text) => WsMessage::Text(bytestring_to_utf8bytes(text)),
        DataMessage::Binary(bin) => WsMessage::Binary(bin),
    }
}

fn utf8bytes_to_bytestring(s: Utf8Bytes) -> ByteString {
    // SAFETY: `Utf8Bytes` and `ByteString` have the same invariant of UTF-8 validity
    unsafe { ByteString::from_bytes_unchecked(Bytes::from(s)) }
}
fn bytestring_to_utf8bytes(s: ByteString) -> Utf8Bytes {
    // SAFETY: `Utf8Bytes` and `ByteString` have the same invariant of UTF-8 validity
    unsafe { Utf8Bytes::from_bytes_unchecked(s.into_bytes()) }
}
