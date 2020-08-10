use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, task::Waker};

/// The message passed between the backend and frontend. Includes
/// associated metadata ensuring that the message is delivered to the
/// intended `WebViewMessageService` and `MessageFuture` waiting for a
/// reply.
#[derive(Deserialize, Serialize, Debug)]
pub struct Message<T> {
    pub subscription_id: u32,
    pub message_id: u32,
    pub inner: T,
}

impl<T> Message<T> {
    pub fn generate_subscription_id() -> u32 {
        rand::thread_rng().gen()
    }

    /// Generate a new message id.
    pub fn generate_message_id() -> u32 {
        rand::thread_rng().gen()
    }

    /// Create a message for the provided subscription id.
    pub fn for_subscription_id(subscription_id: u32, inner: T) -> Self {
        Self {
            subscription_id,
            message_id: Self::generate_message_id(),
            inner,
        }
    }
}

#[cfg(feature = "frontend")]
use wasm_bindgen::prelude::wasm_bindgen;

#[cfg(feature = "frontend")]
#[wasm_bindgen(module = "/src/js/invoke_webview.js")]
extern "C" {
    fn invoke_webview(message: String);
}

/// The waker and the message data for a given message id. When these
/// are set to `Some`, the `Future` waiting for the message will poll
/// `Ready`.
struct WakerMessage<RECV> {
    pub waker: Option<Waker>,
    pub message: Option<Arc<RECV>>,
}

impl<RECV> WakerMessage<RECV> {
    pub fn new() -> Self {
        WakerMessage {
            waker: None,
            message: None,
        }
    }
}

impl<RECV> Default for WakerMessage<RECV> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "frontend")]
pub mod frontend {
    //! This module should be enabled (using the `frontend` feature)
    //! for use in a Rust front-end using the `yew` framework compiled
    //! to WASM, and running in `web-view`.

    pub use super::Message;
    use crate::{invoke_webview, WakerMessage};
    use dashmap::DashMap;
    use serde::{de::DeserializeOwned, Serialize};
    use std::future::Future;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::sync::{Arc, RwLock};
    use std::task::{Context, Poll};
    use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
    use web_sys::{CustomEvent, Document, EventListener, Window};

    /// A map of message ids
    /// ([Message#message_id](Message#message_id)), to the `Waker` for
    /// the task which is waiting, and the message data. When the
    /// [WakerMessage](WakerMessage) data is set, the `Future` waiting
    /// for the message will poll `Ready`.
    type MessageFuturesMap<RECV> = Arc<DashMap<u32, Arc<RwLock<WakerMessage<RECV>>>>>;

    static LISTENER_TYPE: &'static str = "yew-webview-bridge-response";

    /// This is a service that can be used within a `yew` `Component`
    /// to communicate with the backend which is hosting the
    /// `web-view` that the frontend is running in.
    ///
    /// To listen to messages from the backend, this service attaches
    /// a listener to the `document` (`document.addListener(...)`).
    ///
    /// + `<RECV>` is the type type of message this service can receive.
    /// + `<SND>` is the type of message that the backend can recieve
    pub struct WebViewMessageService<RECV, SND> {
        /// A unique identifier for the messages communicating with this service.
        subscription_id: u32,
        message_futures_map: MessageFuturesMap<RECV>,
        event_listener: EventListener,
        _event_listener_closure: Closure<dyn Fn(CustomEvent)>,
        _receive_message_type: PhantomData<SND>,
    }

    impl<RECV, SND> Default for WebViewMessageService<RECV, SND>
    where
        RECV: DeserializeOwned + 'static,
        SND: Serialize,
    {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<RECV, SND> Drop for WebViewMessageService<RECV, SND> {
        /// Removes the event listener.
        fn drop(&mut self) {
            let window: Window = web_sys::window().expect("unable to obtain current Window");
            let document: Document = window
                .document()
                .expect("unable to obtain current Document");
            document
                .remove_event_listener_with_event_listener(LISTENER_TYPE, &self.event_listener)
                .expect("unable to remove yew-webview-bridge-response listener");
        }
    }

    impl<RECV, SND> WebViewMessageService<RECV, SND>
    where
        RECV: DeserializeOwned + 'static,
        SND: Serialize,
    {
        pub fn new() -> Self {
            let subscription_id = Message::<()>::generate_subscription_id();
            let message_futures_map = Arc::new(DashMap::new());

            let window: Window = web_sys::window().expect("unable to obtain current Window");
            let document: Document = window
                .document()
                .expect("unable to obtain current Document");

            let listener_futures_map = message_futures_map.clone();
            let closure: Closure<dyn Fn(CustomEvent)> =
                Closure::wrap(Box::new(move |event: CustomEvent| {
                    Self::response_handler(subscription_id, listener_futures_map.clone(), event);
                }));

            let mut listener = EventListener::new();
            listener.handle_event(closure.as_ref().unchecked_ref());

            document
                .add_event_listener_with_event_listener(LISTENER_TYPE, &listener)
                .expect("unable to register yew-webview-bridge-response callback");

            Self {
                subscription_id,
                message_futures_map,
                event_listener: listener,
                _event_listener_closure: closure,
                _receive_message_type: PhantomData,
            }
        }

        /// Handle an event coming from the `web-view` backend, and
        /// respond to it, resolving/waking the relevent pending
        /// future (with matching message id), if there are any.
        fn response_handler<R>(
            subscription_id: u32,
            message_futures_map: MessageFuturesMap<R>,
            event: CustomEvent,
        ) where
            R: DeserializeOwned,
        {
            let detail: JsValue = event.detail();
            let message_str: String = detail
                .as_string()
                .expect("expected event detail to be a String");
            let message: Message<R> =
                serde_json::from_str(&message_str).expect("unable to parse json from string");

            if message.subscription_id != subscription_id {
                return;
            }
            if !message_futures_map.contains_key(&message.message_id) {
                return;
            }

            let future_value = message_futures_map
                .remove(&message.message_id)
                .expect("unable to remove message from message_futures_map")
                .1;

            let mut future_value_write = future_value
                .write()
                .expect("unable to obtain write lock for WakerMessage in message_futures_map");
            future_value_write.message = Some(Arc::new(message.inner));
            future_value_write.waker.as_ref().map(|waker| {
                waker.wake_by_ref();
            });
        }

        fn new_result_future(&self, message_id: u32) -> MessageFuture<RECV> {
            let future = MessageFuture::new();

            self.message_futures_map
                .insert(message_id, future.message.clone());

            future
        }

        /// Create a new message for this service instance's subscription.
        ///
        /// `<SND>` is the type of the message to be sent.
        fn new_message(&self, message: SND) -> Message<SND> {
            Message::for_subscription_id(self.subscription_id, message)
        }

        /// Send a message to the `web-view` backend, receive a future
        /// for a reply message.
        pub fn send_message(&self, message: SND) -> MessageFuture<RECV> {
            let message = self.new_message(message);
            let message_res = self.new_result_future(message.message_id);

            let message_serialized =
                serde_json::to_string(&message).expect("unable to serialize message");

            #[allow(unused_unsafe)]
            unsafe {
                invoke_webview(message_serialized);
            }

            message_res
        }
    }

    /// A message that will be received in the future.
    pub struct MessageFuture<RECV> {
        message: Arc<RwLock<WakerMessage<RECV>>>,
        return_type: PhantomData<RECV>,
    }

    impl<RECV> MessageFuture<RECV>
    where
        RECV: DeserializeOwned,
    {
        pub fn new() -> Self {
            Self {
                message: Arc::new(RwLock::new(WakerMessage::default())),
                return_type: PhantomData,
            }
        }
    }

    impl<RECV> Future for MessageFuture<RECV>
    where
        RECV: DeserializeOwned,
    {
        type Output = Arc<RECV>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let mut waker_message = self
                .message
                .write()
                .expect("unable to obtain RwLock on message");

            if (*waker_message).message.is_none() {
                waker_message.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }

            let message = waker_message
                .message
                .as_ref()
                .expect("unable to obtain reference to message in WakerMessage");
            Poll::Ready(message.clone())
        }
    }
}

#[cfg(feature = "backend")]
pub mod backend {
    //! This module should be enable (using the `backend` feature) for
    //! use in the the backend of the application which is hosting the
    //! `web-view` window.

    pub use super::Message;
    use serde::{Deserialize, Serialize};
    use web_view::WebView;
    use thiserror::Error;

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
    pub fn handle_yew_message<
        'a,
        T,
        RECV: Deserialize<'a> + Serialize,
        SND: Deserialize<'a> + Serialize,
        H: Fn(RECV) -> Option<SND>,
    >(
        webview: &mut WebView<T>,
        arg: &'a str,
        handler: H,
    ) -> Result<(), YewWebviewBridgeError> {
        let in_message: Message<RECV> =
            serde_json::from_str(&arg).map_err(|err| YewWebviewBridgeError::UnableToDeserialize(err))?;

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
    pub fn send_response_to_yew<T, M: Serialize>(webview: &mut WebView<T>, message: Message<M>) -> Result<(), YewWebviewBridgeError> {
        let message_string =
            serde_json::to_string(&message).map_err(|err| YewWebviewBridgeError::UnableToSerialize(err))?;
        let eval_script = format!(
            r#"
        document.dispatchEvent(
            new CustomEvent("{event_name}", {{ detail: {message:?} }})
        );"#,
            event_name = "yew-webview-bridge-response",
            message = message_string
        );
        
        webview
            .eval(&eval_script)?;

        Ok(())
    }
}
