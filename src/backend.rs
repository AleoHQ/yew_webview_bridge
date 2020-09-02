//! This module should be enable (using the `backend` feature) for
//! use in the the backend of the application which is hosting the
//! `web-view` window.

pub use super::Message;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::net::TcpListener;
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

pub fn run_websocket_bridge<'a, RECV, SND, H>(listener: TcpListener, message_handler: H)
where
    RECV: DeserializeOwned + Serialize,
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
                    log::error!(
                        target: "yew_webview_bridge::websocket_bridge",
                        "Error during connection: {}",
                        error);
                    return;
                }
            };
            let mut websocket = match tungstenite::accept(connection) {
                Ok(websocket) => websocket,
                Err(error) => {
                    log::error!(
                        target: "yew_webview_bridge::websocket_bridge",
                        "Error during handshake: {}",
                        error);
                    return;
                }
            };

            'read_messages: loop {
                let msg = match websocket.read_message() {
                    Ok(msg) => msg,
                    Err(error) => {
                        log::error!(
                            target: "yew_webview_bridge::websocket_bridge", 
                            "Error reading received message: {}", 
                            error);
                        continue 'read_messages;
                    }
                };

                match msg {
                    tungstenite::Message::Text(message_string) => {
                        let recv: RECV = match serde_json::from_str(&message_string) {
                            Ok(recv) => recv,
                            Err(error) => {
                                log::error!(
                                    target: "yew_webview_bridge::websocket_bridge", 
                                    "Error deserializing received message: {}", 
                                    error);
                                continue 'read_messages;
                            }
                        };

                        let snd = match thread_message_handler(recv) {
                            Some(snd) => snd,
                            None => {
                                continue 'read_messages;
                            }
                        };

                        let send_string = match serde_json::to_string(&snd) {
                            Ok(string) => string,
                            Err(error) => {
                                log::error!(
                                    target: "yew_webview_bridge::websocket_bridge", 
                                    "Error serializing reply message: {}", 
                                    error);
                                continue 'read_messages;
                            }
                        };

                        let msg = tungstenite::Message::Text(send_string);

                        match websocket.write_message(msg) {
                            Ok(_) => {}
                            Err(error) => {
                                log::error!(
                                        target: "yew_webview_bridge::websocket_bridge", 
                                        "Error writing reply message: {}", 
                                        error);
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
