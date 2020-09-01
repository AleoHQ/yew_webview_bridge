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
    /// Generate a new subscription id.
    pub fn generate_subscription_id() -> u32 {
        // TODO: revert if required This is now using OsRng instead of
        // ThreadRng due to a spurious crash in rand
        // https://github.com/rust-random/rand/issues/1016
        //
        // Using OsRng here may be a performance regression, due to a
        // need to shell out to javascript to obtain the value. I may
        // switch to using StdRng in a lazy_static instance instead
        // for a while after this to see if that will solve the issue.
        rand::rngs::OsRng.gen()
    }

    /// Generate a new message id.
    pub fn generate_message_id() -> u32 {
        // TODO: revert if required.
        // See generate_subscription_id() for more information.
        rand::rngs::OsRng.gen()
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
pub mod frontend;

#[cfg(feature = "backend")]
pub mod backend;
