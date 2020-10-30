//! This module should be enabled (using the `frontend` feature)
//! for use in a Rust front-end using the `yew` framework compiled
//! to WASM, and running in `web-view`.

pub use super::{Message, MessageError};
use crate::{MessageAction, MessageErrorType, MessageWaker};

use dashmap::DashMap;
use serde::{de::DeserializeOwned, Serialize};
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{CustomEvent, Document, EventListener, External, MessageEvent, WebSocket, Window};

use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::{
    cell::RefCell,
    rc::Rc,
    task::{Context, Poll},
};
use std::{fmt::Display, future::Future};

#[derive(Debug)]
struct JsError {
    value: JsValue,
}

impl Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.value)
    }
}

impl From<JsValue> for JsError {
    fn from(value: JsValue) -> Self {
        Self { value }
    }
}

impl std::error::Error for JsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// Run the `web-view` defined `window.external.invoke()` method.
pub fn invoke_webview(message: String) {
    let window: Window = if let Some(window) = web_sys::window() {
        window
    } else {
        tracing::error!("Unable to obtain current Window");
        return;
    };

    let external: External = match window.external() {
        Ok(external) => external,
        Err(error) => {
            tracing::error!("Unable to obtain window.external because: {:?}", error);
            return;
        }
    };

    let external_value: JsValue = external.into();
    let invoke_property_name = JsValue::from_str("invoke");
    #[allow(unused_unsafe)]
    let invoke_property =
        match unsafe { js_sys::Reflect::get(&external_value, &invoke_property_name) } {
            Ok(prop) => prop,
            Err(error) => {
                tracing::error!("Unable to find external.invoke(): {:?}", error);
                return;
            }
        };

    let invoke_fn: js_sys::Function = match invoke_property.dyn_into::<js_sys::Function>() {
        Ok(invoke_fn) => invoke_fn,
        Err(error) => {
            tracing::error!("Unable to cast window.invoke() to a Function: {:?}", error);
            return;
        }
    };

    let message_value = JsValue::from_str(&message);

    if let Err(error) = invoke_fn.call1(&JsValue::NULL, &message_value) {
        tracing::error!("Error while calling window.invoke(): {:?}", error);
        return;
    }
}

/// Check whether the `window.external.invoke()` method exists, and
/// thus whether this code is running in a `web-view` context.
pub fn invoke_webview_exists() -> bool {
    let window: Window = if let Some(window) = web_sys::window() {
        window
    } else {
        tracing::error!("Unable to obtain current Window");
        return false;
    };

    let external: External = match window.external() {
        Ok(external) => external,
        Err(error) => {
            tracing::error!("Unable to obtain window.external because: {:?}", error);
            return false;
        }
    };

    let external_value: JsValue = external.into();
    let invoke_property_name = JsValue::from_str("invoke");
    #[allow(unused_unsafe)]
    match unsafe { js_sys::Reflect::has(&external_value, &invoke_property_name) } {
        Ok(exists) => {
            tracing::debug!("Successfully checked external.invoke(), exists: {}", exists);
            exists
        }
        Err(error) => {
            tracing::error!(
                "Error checking whether external.invoke() exists: {:?}",
                error
            );
            false
        }
    }
}

/// A map of message ids
/// ([Message#message_id](Message#message_id)), to the `Waker` for
/// the task which is waiting, and the message data. When the
/// [WakerMessage](WakerMessage) data is set, the `Future` waiting
/// for the message will poll `Ready`.
type MessageFuturesMap<RECV> = Arc<DashMap<u32, Arc<RwLock<MessageWaker<RECV>>>>>;

static LISTENER_TYPE: &str = "yew-webview-bridge-response";

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
        let using_webview = invoke_webview_exists();

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
            .insert(message.message_id, future.message_waker.clone());

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
            .insert(message.message_id, future.message_waker.clone());

        let send_result: Result<(), MessageError> = match self.websocket.ready_state() {
            // CONNECTING
            0 => {
                self.queued.borrow_mut().push(message);
                Ok(())
            }
            // OPEN
            1 => self.send_message_impl(&message),
            // CLOSING or CLOSED
            2 | 3 => Err(MessageError::new(
                MessageAction::Sending,
                MessageErrorType::ConnectionClosed,
            )),
            unknown => {
                panic!("Unknown websocket ready_state: {}", unknown);
            }
        };

        if let Err(error) = send_result {
            future
                .message_waker
                .write()
                .expect("unable to obtain write lock for MessageWaker in message_futures_map")
                .message_result = Some(Arc::new(Err(error)));
        }

        future
    }

    fn send_message_impl(&self, message: &Message<SND>) -> Result<(), MessageError> {
        let message_serialized = serde_json::to_string(message).map_err(|json_error| {
            MessageError::new(MessageAction::Sending, MessageErrorType::UnableToSerialize)
                .with_source(json_error)
        })?;
        if let Err(error) = self.websocket.send_with_str(&message_serialized) {
            return Err(
                MessageError::new(MessageAction::Sending, MessageErrorType::Websocket)
                    .with_source(JsError::from(error)),
            );
        }
        Ok(())
    }

    fn send_queue(&self) {
        match self.websocket.ready_state() {
            // CONNECTING
            0 => {}
            // OPEN
            1 => {
                self.queued.borrow().iter().for_each(|message| {
                    if let Err(error) = self.send_message_impl(message) {
                        let message_waker_ref = self.message_futures_map.get_mut(&message.message_id)
                            .unwrap_or_else(|| panic!("Expected message {} to have a corresponding \
                                                       waker in the `message_futures_map`.", 
                                                       message.message_id));
                        message_waker_ref.value()
                            .write()
                            .expect("unable to write to value in message_futures_map")
                            .message_result = Some(Arc::new(Err(error)));
                    }
                });
            }
            // CLOSING or CLOSED
            2 | 3 => {
                self.queued.borrow().iter().for_each(|message| {
                    let message_waker_ref = self.message_futures_map.get_mut(&message.message_id)
                    .unwrap_or_else(|| panic!("Expected message {} to have a corresponding \
                                               waker in the `message_futures_map`.", 
                                               message.message_id));

                    let error = MessageError::new(MessageAction::Sending, MessageErrorType::ConnectionClosed);
                    message_waker_ref.value()
                        .write()
                        .expect("unable to write to value in message_futures_map")
                        .message_result = Some(Arc::new(Err(error)));
                });
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
            tracing::debug!("websocket opened");
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
                    &message_string,
                );
            } else {
                tracing::error!("message event, received Unknown: {:?}", event.data());
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        callback_data
            .websocket
            .set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));

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
            tracing::error!(target: "yew_webview_bridge", "Error while closing websocket: {:?}", error)
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

/// Handle an event coming from the `web-view` backend, and respond to
/// it (if the event matches the specified `subscription_id`),
/// resolving/waking the relevent pending future (with matching
/// message id), if there are any.
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
        .expect("unable to obtain write lock for MessageWaker in message_futures_map");
    future_value_write.message_result = Some(Arc::new(Ok(message.inner)));
    future_value_write.waker.as_ref().map(|waker| {
        waker.wake_by_ref();
    });
}

/// A message that will be received in the future.
pub struct MessageFuture<RECV> {
    message_waker: Arc<RwLock<MessageWaker<RECV>>>,
    return_type: PhantomData<RECV>,
}

impl<RECV> MessageFuture<RECV>
where
    RECV: DeserializeOwned,
{
    pub fn new() -> Self {
        Self {
            message_waker: Arc::new(RwLock::new(MessageWaker::default())),
            return_type: PhantomData,
        }
    }
}

impl<RECV> Future for MessageFuture<RECV>
where
    RECV: DeserializeOwned,
{
    type Output = Arc<Result<RECV, MessageError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut message_waker = self
            .message_waker
            .write()
            .expect("unable to obtain RwLock on MessageWaker instance");

        // Set the waker for this future so that it can be woken later
        // when the message is received in `response_handler`.
        if (*message_waker).message_result.is_none() {
            message_waker.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let message_result = message_waker
            .message_result
            .as_ref()
            .expect("unable to obtain reference to message in MessageWaker");
        Poll::Ready(message_result.clone())
    }
}
