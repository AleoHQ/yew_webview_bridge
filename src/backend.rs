//! This module should be enable (using the `backend` feature) for
//! use in the the backend of the application which is hosting the
//! `web-view` window.

pub use super::Message;
use async_std::{sync::Mutex, task};
use future::Future;
use futures_util::{future, SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{fmt::Debug, pin::Pin, sync::Arc};
use thiserror::Error;
use web_view::WebView;

#[derive(Error, Debug)]
pub enum YewWebviewBridgeError {
    #[error("Unable to serialize outgoing message to json string: {0}")]
    UnableToSerialize(serde_json::Error),
    #[error("Unable to deserialize incoming json string: {0}")]
    UnableToDeserialize(serde_json::Error),
    #[error("Failed to dispatch message to frontend.")]
    FieldToDispatch(#[from] web_view::Error),
}

/// Handle a `web-view` message as a message coming from `yew`.
///
/// + `<RECV>` is the type of message being received from a
///   `WebViewMessageService` in the frontend.
/// + `<SND>` is the type of message expected by the
///   `WebViewMessageService` in the frontend, in the reply to the
///   `RECV` message.
///
/// # Example
///
/// ```no_run
/// use yew_webview_bridge::backend::handle_yew_message;
/// use web_view;
///
/// web_view::builder()
///     .content(web_view::Content::Html("<html></html>"))
///     .user_data(())
///     .invoke_handler(|webview, arg| {
///         handle_yew_message(webview, arg, |message: String| {
///             // If we return a Some(_), we send a response for this message
///             Some("reply".to_string())
///         });
///         Ok(())
///     });
/// ```
pub fn handle_yew_message<'a, T, RECV, SND, H>(
    webview: &mut WebView<T>,
    arg: &'a str,
    handler: H,
) -> Result<(), YewWebviewBridgeError>
where
    RECV: Deserialize<'a> + Serialize,
    SND: Deserialize<'a> + Serialize,
    H: Fn(RECV) -> Option<SND>,
{
    let in_message: Message<RECV> = serde_json::from_str(&arg)
        .map_err(|err| YewWebviewBridgeError::UnableToDeserialize(err))?;

    let output = handler(in_message.inner);
    if let Some(response) = output {
        let out_message = Message {
            subscription_id: in_message.subscription_id,
            message_id: in_message.message_id,
            inner: response,
        };

        send_response_to_yew(webview, out_message)?;
    }

    Ok(())
}

/// Send a response a [Message](Message) recieved from the frontend via
/// `web-view`'s `eval()` method.
pub fn send_response_to_yew<T, M: Serialize>(
    webview: &mut WebView<T>,
    message: Message<M>,
) -> Result<(), YewWebviewBridgeError> {
    let message_string = serde_json::to_string(&message)
        .map_err(|err| YewWebviewBridgeError::UnableToSerialize(err))?;
    let eval_script = format!(
        r#"
    document.dispatchEvent(
        new CustomEvent("{event_name}", {{ detail: {message:?} }})
    );"#,
        event_name = "yew-webview-bridge-response",
        message = message_string
    );

    webview.eval(&eval_script)?;

    Ok(())
}

/// An async version of [run_websocket_bridge], using the `async-std`
/// runtime. `concurrent_limit` is an optional limit on the number of
/// messages that can be handled concurrently for a given connection.
pub async fn run_websocket_bridge_async<'a, RECV, SND, H>(
    concurrent_limit: impl Into<Option<usize>>,
    listener: async_std::net::TcpListener,
    message_handler: H,
) where
    RECV: DeserializeOwned + Debug + Send + 'static,
    SND: Serialize + Send + 'static,
    H: Fn(RECV) -> Pin<Box<dyn Future<Output = Option<SND>> + Send>>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let concurrent_limit: Option<usize> = concurrent_limit.into();
    'connections: loop {
        let (connection, _address) = match listener.accept().await {
            Ok(ok) => ok,
            Err(error) => {
                log::error!("Error during connection: {}", error);
                continue 'connections;
            }
        };

        let conn_msg_handler = message_handler.clone();
        async_std::task::spawn(async move {
            let ws_stream = match async_tungstenite::accept_async(connection).await {
                Ok(ws) => ws,
                Err(error) => {
                    log::error!("Error during handshake: {}", error);
                    return;
                }
            };

            let (ws_out, ws_in) = ws_stream.split();
            let ws_out = Arc::new(Mutex::new(ws_out));

            let ws_in_msg_handler = conn_msg_handler.clone();
            ws_in
                .for_each_concurrent(concurrent_limit, move |msg_result| {
                    let fut_msg_handler = ws_in_msg_handler.clone();
                    let fut_ws_out = ws_out.clone();
                    async move {
                        let msg = match msg_result {
                            Ok(msg) => msg,
                            Err(error) => {
                                log::error!("Error reading received message: {}", error);
                                return;
                            }
                        };

                        match msg {
                            tungstenite::Message::Text(message_string) => {
                                let task_msg_handler = fut_msg_handler.clone();
                                let de_fut =
                                    task::spawn(
                                        async move { serde_json::from_str(&message_string) },
                                    )
                                    .await;

                                let in_message: Message<RECV> = match de_fut {
                                    Ok(recv) => recv,
                                    Err(error) => {
                                        log::error!(
                                            "Error deserializing received message: {}",
                                            error
                                        );
                                        return;
                                    }
                                };

                                let response: Option<SND> =
                                    task_msg_handler(in_message.inner).await;

                                let out_message = Message {
                                    subscription_id: in_message.subscription_id,
                                    message_id: in_message.message_id,
                                    inner: response,
                                };

                                let send_string = match serde_json::to_string(&out_message) {
                                    Ok(string) => string,
                                    Err(error) => {
                                        log::error!("Error serializing reply message: {}", error);
                                        return;
                                    }
                                };

                                let msg = tungstenite::Message::Text(send_string);

                                match fut_ws_out.lock().await.send(msg).await {
                                    Ok(_) => {}
                                    Err(error) => {
                                        log::error!("Error writing reply message: {}", error);
                                    }
                                }
                            }
                            tungstenite::Message::Binary(_) => {}
                            tungstenite::Message::Ping(_) => {}
                            tungstenite::Message::Pong(_) => {}
                            tungstenite::Message::Close(_) => {
                                return;
                            }
                        }
                    }
                })
                .await;
        });
    }
}

/// Run a websocket server that handles messages using the provided
/// `message_handler`. Connections are handled in parallel, but
/// messages per connection are currently handled synchronously.
pub fn run_websocket_bridge<'a, RECV, SND, H>(listener: std::net::TcpListener, message_handler: H)
where
    RECV: DeserializeOwned + Serialize + Debug,
    SND: Deserialize<'a> + Serialize,
    H: Fn(RECV) -> Option<SND> + Send + Clone + 'static,
{
    // For each incoming connection spawn a new thread to handle it.
    // that should scale well enough for debug purposes, where the
    // expected number of connections is 1.
    for connection in listener.incoming() {
        let thread_message_handler = message_handler.clone();
        std::thread::spawn(move || {
            let connection = match connection {
                Ok(connection) => connection,
                Err(error) => {
                    log::error!("Error during connection: {}", error);
                    return;
                }
            };
            let mut websocket = match tungstenite::accept(connection) {
                Ok(websocket) => websocket,
                Err(error) => {
                    log::error!("Error during handshake: {}", error);
                    return;
                }
            };

            'read_messages: loop {
                let msg = match websocket.read_message() {
                    Ok(msg) => msg,
                    Err(error) => {
                        log::error!("Error reading received message: {}", error);
                        continue 'read_messages;
                    }
                };

                match msg {
                    tungstenite::Message::Text(message_string) => {
                        let in_message: Message<RECV> = match serde_json::from_str(&message_string)
                        {
                            Ok(recv) => recv,
                            Err(error) => {
                                log::error!("Error deserializing received message: {}", error);
                                continue 'read_messages;
                            }
                        };

                        let response = match thread_message_handler(in_message.inner) {
                            Some(response) => response,
                            None => {
                                continue 'read_messages;
                            }
                        };

                        let out_message = Message {
                            subscription_id: in_message.subscription_id,
                            message_id: in_message.message_id,
                            inner: response,
                        };

                        let send_string = match serde_json::to_string(&out_message) {
                            Ok(string) => string,
                            Err(error) => {
                                log::error!("Error serializing reply message: {}", error);
                                continue 'read_messages;
                            }
                        };

                        let msg = tungstenite::Message::Text(send_string);

                        match websocket.write_message(msg) {
                            Ok(_) => {}
                            Err(error) => {
                                log::error!("Error writing reply message: {}", error);
                            }
                        }
                    }
                    tungstenite::Message::Binary(_) => {}
                    tungstenite::Message::Ping(_) => {}
                    tungstenite::Message::Pong(_) => {}
                    tungstenite::Message::Close(_close_frame) => {
                        return;
                    }
                }
            }
        });
    }
}
