//! This module should be enabled (using the `frontend` feature)
//! for use in a Rust front-end using the `yew` framework compiled
//! to WASM, and running in `web-view`.

pub use super::Message;
use crate::WakerMessage;
use dashmap::DashMap;
use serde::{de::DeserializeOwned, Serialize};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::{task::{Context, Poll}, rc::Rc};
use wasm_bindgen::{
    prelude::{wasm_bindgen, Closure},
    JsCast, JsValue,
};
use web_sys::{CustomEvent, Document, EventListener, Window, WebSocket};

#[wasm_bindgen(module = "/src/js/invoke_webview.js")]
extern "C" {
    fn invoke_webview(message: String);
}

#[wasm_bindgen(module = "/src/js/invoke_webview_exists.js")]
extern "C" {
    fn invoke_webview_exists() -> bool;
}

/// A map of message ids
/// ([Message#message_id](Message#message_id)), to the `Waker` for
/// the task which is waiting, and the message data. When the
/// [WakerMessage](WakerMessage) data is set, the `Future` waiting
/// for the message will poll `Ready`.
type MessageFuturesMap<RECV> = Arc<DashMap<u32, Arc<RwLock<WakerMessage<RECV>>>>>;

static LISTENER_TYPE: &'static str = "yew-webview-bridge-response";

enum Connection<RECV, SND> {
    Webview(WebviewConnData),
    Websocket(WebsocketConnData<RECV, SND>),
}

enum ConnectionMessageData<RECV, SND> {
    Webview,
    Websocket(Arc<WebsocketSendMessageData<RECV, SND>>)
}

struct WebviewConnData {
    event_listener: EventListener,
    _event_listener_closure: Closure<dyn Fn(CustomEvent)>,
}

struct WebsocketConnData<RECV, SND> {
    onopen_callback: Closure<dyn FnMut(JsValue)>,
    message_data: Arc<WebsocketSendMessageData<RECV, SND>>,
}

struct WebsocketSendMessageData<RECV, SND> {
    subscription_id: u32,
    message_futures_map: MessageFuturesMap<RECV>,
    queued: Rc<Vec<SND>>,
    websocket: WebSocket,
}

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
    connection: Connection<RECV, SND>,
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
        match &self.connection {
            Connection::Webview(data) => {
                let window: Window = web_sys::window().expect("unable to obtain current Window");
                let document: Document = window
                    .document()
                    .expect("unable to obtain current Document");
                document
                    .remove_event_listener_with_event_listener(LISTENER_TYPE, &data.event_listener)
                    .expect("unable to remove yew-webview-bridge-response listener");
            }
            Connection::Websocket(data) => {
                if let Err(error) = data.message_data.websocket.close_with_code(1000) {
                    log::error!(target: "yew_webview_bridge", "Error while closing websocket: {:?}", error)
                }
            }
        }
    }
}

impl<RECV, SND> WebViewMessageService<RECV, SND>
where
    RECV: DeserializeOwned + 'static,
    SND: Serialize,
{
    pub fn new() -> Self {
        let subscription_id = Message::<()>::generate_subscription_id();
        let message_futures_map = MessageFuturesMap::default();

        #[allow(unused_unsafe)]
        let using_webview = unsafe { invoke_webview_exists() };

        log::debug!("using_webview: {}", using_webview);

        let connection = if using_webview {
            Connection::Webview(Self::setup_webview_listener(
                &message_futures_map,
                subscription_id,
            ))
        } else {
            let window = web_sys::window().expect("unable to obtain current Window");
            let window_value: JsValue = window.into();
            let address_property = JsValue::from_str("bridge_address");
            let bridge_address = unsafe {js_sys::Reflect::get(&window_value, &address_property).expect("unable to read window.bridge_address")};
            let websocket_address = format!("ws://{}", bridge_address.as_string().expect("expected window.bridge_address to be a string"));
            let ws: WebSocket = WebSocket::new(&websocket_address).expect("problem creating web socket");
            let queued: Rc<Vec<SND>> = Rc::default();

            let message_data = Arc::new(WebsocketSendMessageData {
                websocket: ws,
                subscription_id,
                queued,
                message_futures_map: message_futures_map.clone(),
            });

            let message_data_onopen = message_data.clone();
            let onopen_callback = Closure::wrap(Box::new(move |_| {
                log::debug!("websocket opened");
            }) as Box<dyn FnMut(JsValue)>);

            message_data.websocket.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));

            let data = WebsocketConnData{ 
                onopen_callback,
                message_data,
            };

            Connection::Websocket(data)
        };

        Self {
            subscription_id,
            message_futures_map,
            connection,
            _receive_message_type: PhantomData,
        }
    }

    fn setup_webview_listener(
        message_futures_map: &MessageFuturesMap<RECV>,
        subscription_id: u32,
    ) -> WebviewConnData {
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

        WebviewConnData {
            event_listener: listener,
            _event_listener_closure: closure,
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
        let message: Message<R> = match serde_json::from_str(&message_str) {
            Ok(message) => message,
            Err(error) => panic!(
                "Error while parsing message: {}. Message contents: {}",
                error, &message_str
            ),
        };

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

    fn new_result_future(message_futures_map: &MessageFuturesMap<RECV>, message_id: u32) -> MessageFuture<RECV> {
        let future = MessageFuture::new();

        message_futures_map
            .insert(message_id, future.message.clone());

        future
    }

    /// Create a new message for this service instance's subscription.
    ///
    /// `<SND>` is the type of the message to be sent.
    fn new_message(subscription_id: u32, message: SND) -> Message<SND> {
        Message::for_subscription_id(subscription_id, message)
    }

    /// Send a message to the `web-view` backend, receive a future
    /// for a reply message.
    pub fn send_message(&self, message: SND) -> MessageFuture<RECV> {
        let message_data = match &self.connection {
            Connection::Webview(_) => {
                ConnectionMessageData::Webview
            }
            Connection::Websocket(data) => {
                ConnectionMessageData::Websocket(data.message_data.clone())
            }
        };
        Self::send_message_impl(&message_data, self.subscription_id, &self.message_futures_map, message)
    }

    fn send_message_impl(
        message_data: &ConnectionMessageData<RECV, SND>,
        subscription_id: u32, 
        message_futures_map: &MessageFuturesMap<RECV>, 
        message: SND) -> MessageFuture<RECV> {
        let message = Self::new_message(subscription_id, message);
        let message_res = Self::new_result_future(message_futures_map, message.message_id);

        let message_serialized =
            serde_json::to_string(&message).expect("unable to serialize message");

        match message_data {
            ConnectionMessageData::Webview => {
                #[allow(unused_unsafe)]
                unsafe {
                    invoke_webview(message_serialized);
                }
            }
            ConnectionMessageData::Websocket(data) => {
                if let Err(error) = data.websocket.send_with_str(&message_serialized) {
                    log::error!(target: "yew_webview_bridge", "Error while sending message: {:?}", error);
                }
            }
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
