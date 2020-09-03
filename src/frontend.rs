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
use std::{
    cell::RefCell,
    rc::Rc,
    task::{Context, Poll},
};
use wasm_bindgen::{
    prelude::{wasm_bindgen, Closure},
    JsCast, JsValue,
};
use web_sys::{CustomEvent, Document, EventListener, WebSocket, Window, MessageEvent};

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

enum ServiceType<RECV, SND> {
    Webview(WebviewClosureMessageService<RECV, SND>),
    Websocket(WebsocketMessageService<RECV, SND>),
}

impl<RECV, SND> ServiceType<RECV, SND>
where
    SND: Serialize,
    RECV: DeserializeOwned,
{
    pub fn service(&self) -> &dyn MessageService<RECV, SND> {
        match self {
            ServiceType::Webview(service) => service,
            ServiceType::Websocket(service) => service,
        }
    }
}

pub trait MessageService<RECV, SND> {
    fn send_message(&self, message: SND) -> MessageFuture<RECV>;
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
    service: ServiceType<RECV, SND>,
}

impl<RECV, SND> WebViewMessageService<RECV, SND>
where
    RECV: DeserializeOwned + 'static,
    SND: Serialize + 'static,
{
    pub fn new() -> Self {
        #[allow(unused_unsafe)]
        let using_webview = unsafe { invoke_webview_exists() };

        log::debug!("using_webview: {}", using_webview);

        let service = if using_webview {
            ServiceType::Webview(WebviewClosureMessageService::new())
        } else {
            ServiceType::Websocket(WebsocketMessageService::new())
        };

        Self { service }
    }

    /// Send a message to the `web-view` backend, receive a future
    /// for a reply message.
    pub fn send_message(&self, message: SND) -> MessageFuture<RECV> {
        self.service.service().send_message(message)
    }
}

impl<RECV, SND> Default for WebViewMessageService<RECV, SND>
where
    RECV: DeserializeOwned + 'static,
    SND: Serialize + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

struct WebviewClosureMessageService<RECV, SND> {
    subscription_id: u32,
    message_futures_map: MessageFuturesMap<RECV>,
    event_listener: EventListener,
    _event_listener_closure: Closure<dyn Fn(CustomEvent)>,
    _receive_message_type: PhantomData<SND>,
}

impl<RECV, SND> WebviewClosureMessageService<RECV, SND>
where
    RECV: DeserializeOwned + 'static,
{
    pub fn new() -> Self {
        let subscription_id = Message::<()>::generate_subscription_id();
        let message_futures_map = MessageFuturesMap::default();

        let window: Window = web_sys::window().expect("unable to obtain current Window");
        let document: Document = window
            .document()
            .expect("unable to obtain current Document");

        let listener_futures_map = message_futures_map.clone();
        let closure: Closure<dyn Fn(CustomEvent)> =
            Closure::wrap(Box::new(move |event: CustomEvent| {
                let detail: JsValue = event.detail();
                let message_string: String = detail
                    .as_string()
                    .expect("expected event detail to be a String");
                response_handler::<RECV>(subscription_id, &listener_futures_map, &message_string);
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
}

impl<RECV, SND> MessageService<RECV, SND> for WebviewClosureMessageService<RECV, SND>
where
    SND: Serialize,
    RECV: DeserializeOwned,
{
    fn send_message(&self, message: SND) -> MessageFuture<RECV> {
        let message = Message::for_subscription_id(self.subscription_id, message);

        let future = MessageFuture::new();
        self.message_futures_map
            .insert(message.message_id, future.message.clone());

        let message_serialized =
            serde_json::to_string(&message).expect("unable to serialize message");

        #[allow(unused_unsafe)]
        unsafe {
            invoke_webview(message_serialized);
        }

        future
    }
}

impl<RECV, SND> Drop for WebviewClosureMessageService<RECV, SND> {
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

struct WebsocketSendMessageData<RECV, SND> {
    subscription_id: u32,
    message_futures_map: MessageFuturesMap<RECV>,
    queued: Rc<RefCell<Vec<Message<SND>>>>,
    websocket: WebSocket,
}

impl<RECV, SND> WebsocketSendMessageData<RECV, SND>
where
    SND: Serialize,
    RECV: DeserializeOwned,
{
    fn send_message(&self, message: SND) -> MessageFuture<RECV> {
        let message = Message::for_subscription_id(self.subscription_id, message);

        let future = MessageFuture::new();
        self.message_futures_map
            .insert(message.message_id, future.message.clone());

        match self.websocket.ready_state() {
            // CONNECTING
            0 => self.queued.borrow_mut().push(message),
            // OPEN
            1 => {
                self.send_message_impl(&message);
            }
            // CLOSING or CLOSED
            2 | 3 => log::error!("Cannot send message, websocket is already closing/closed"),
            unknown => {
                panic!("Unknown websocket ready_state: {}", unknown);
            }
        }

        future
    }

    fn send_message_impl(&self, message: &Message<SND>) {
        let message_serialized =
            serde_json::to_string(message).expect("unable to serialize message");
        if let Err(error) = self.websocket.send_with_str(&message_serialized) {
            log::error!(target: "yew_webview_bridge", "Error while sending message: {:?}", error);
        }
    }

    fn send_queue(&self) {
        match self.websocket.ready_state() {
            // CONNECTING
            0 => {}
            // OPEN
            1 => {
                for message in self.queued.borrow().iter() {
                    self.send_message_impl(message);
                }
            }
            // CLOSING or CLOSED
            2 | 3 => {
                log::error!("Cannot send messages in queue, websocket is already closing/closed")
            }
            unknown => {
                panic!("Unknown websocket ready_state: {}", unknown);
            }
        }
    }
}
struct WebsocketMessageService<RECV, SND> {
    callback_data: Arc<WebsocketSendMessageData<RECV, SND>>,
    _onmessage_callback: Closure<dyn FnMut(MessageEvent)>,
    _onopen_callback: Closure<dyn FnMut(JsValue)>,
    _receive_message_type: PhantomData<SND>,
}

impl<RECV, SND> WebsocketMessageService<RECV, SND>
where
    SND: Serialize + 'static,
    RECV: DeserializeOwned + 'static,
{
    pub fn new() -> Self {
        let subscription_id = Message::<()>::generate_subscription_id();
        let message_futures_map = MessageFuturesMap::default();

        let window = web_sys::window().expect("unable to obtain current Window");
        let window_value: JsValue = window.into();
        let address_property = JsValue::from_str("bridge_address");

        #[allow(unused_unsafe)]
        let bridge_address = unsafe {
            js_sys::Reflect::get(&window_value, &address_property)
                .expect("unable to read window.bridge_address")
        };
        let websocket_address = format!(
            "ws://{}",
            bridge_address
                .as_string()
                .expect("expected window.bridge_address to be a string")
        );
        let ws: WebSocket =
            WebSocket::new(&websocket_address).expect("problem creating web socket");
        let queued: Rc<RefCell<Vec<Message<SND>>>> = Rc::default();

        let callback_data = Arc::new(WebsocketSendMessageData {
            websocket: ws,
            subscription_id,
            queued,
            message_futures_map: message_futures_map.clone(),
        });


        let onopen_data = callback_data.clone();
        let onopen_callback = Closure::wrap(Box::new(move |_| {
            log::debug!("websocket opened");
            onopen_data.send_queue();
        }) as Box<dyn FnMut(JsValue)>);
        callback_data
            .websocket
            .set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));

        let onmessage_data = callback_data.clone();
        let onmessage_callback = Closure::wrap(Box::new(move |event: MessageEvent| {
            // Handle difference Text/Binary,...
            if let Ok(message) = event.data().dyn_into::<js_sys::JsString>() {
                let message_string: String = message.into();

                response_handler::<RECV>(
                    onmessage_data.subscription_id, 
                    &onmessage_data.message_futures_map, 
                    &message_string);
            } else {
                log::error!("message event, received Unknown: {:?}", event.data());
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        callback_data.websocket.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));

        Self {
            callback_data,
            _onmessage_callback: onmessage_callback,
            _onopen_callback: onopen_callback,
            _receive_message_type: PhantomData,
        }
    }
}

impl<RECV, SND> Drop for WebsocketMessageService<RECV, SND> {
    /// Removes the event listener.
    fn drop(&mut self) {
        if let Err(error) = self.callback_data.websocket.close_with_code(1000) {
            log::error!(target: "yew_webview_bridge", "Error while closing websocket: {:?}", error)
        }
    }
}

impl<RECV, SND> MessageService<RECV, SND> for WebsocketMessageService<RECV, SND>
where
    SND: Serialize,
    RECV: DeserializeOwned,
{
    fn send_message(&self, message: SND) -> MessageFuture<RECV> {
        self.callback_data.send_message(message)
    }
}

/// Handle an event coming from the `web-view` backend, and
/// respond to it, resolving/waking the relevent pending
/// future (with matching message id), if there are any.
fn response_handler<R>(
    subscription_id: u32,
    message_futures_map: &MessageFuturesMap<R>,
    message_str: &str,
) where
    R: DeserializeOwned,
{
    let message: Message<R> = match serde_json::from_str(message_str) {
        Ok(message) => message,
        Err(error) => panic!(
            "Error while parsing message: {}. Message contents: {}",
            error, message_str
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
