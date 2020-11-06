use rand::Rng;
use serde::{Deserialize, Serialize};

use std::{fmt::Display, sync::Arc, task::Waker};

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

enum MessageWakerResult<RECV> {
    Ok(Arc<RECV>),
    Err(Arc<MessageError>),
    None,
}

impl<RECV> MessageWakerResult<RECV> {
    pub fn is_none(&self) -> bool {
        match self {
            MessageWakerResult::None => true,
            _ => false,
        }
    }

    pub fn expect_result(&self, message: &str) -> MessageResult<RECV> {
        match self {
            MessageWakerResult::Ok(ok) => Ok(ok.clone()),
            MessageWakerResult::Err(err) => Err((**err).clone()),
            MessageWakerResult::None => panic!("{}", message),
        }
    }
}

impl<RECV> Default for MessageWakerResult<RECV> {
    fn default() -> Self {
        Self::None
    }
}

pub type MessageResult<RECV> = Result<Arc<RECV>, MessageError>;

/// The waker and the message data for a given message id. When these
/// are set to `Some`, the `Future` waiting for the message will poll
/// `Ready`.
struct MessageWaker<RECV> {
    pub waker: Option<Waker>,
    pub message_result: MessageWakerResult<RECV>,
}

impl<RECV> MessageWaker<RECV> {
    pub fn new() -> Self {
        MessageWaker {
            waker: None,
            message_result: MessageWakerResult::None,
        }
    }
}

impl<RECV> Default for MessageWaker<RECV> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct MessageError {
    error_type: MessageErrorType,
    action: MessageAction,
    source: Option<Arc<(dyn std::error::Error + 'static)>>,
}

impl MessageError {
    pub(crate) fn new(action: MessageAction, error_type: MessageErrorType) -> Self {
        Self {
            error_type,
            action,
            source: None,
        }
    }

    pub(crate) fn with_source<C: std::error::Error + 'static>(mut self, source: C) -> Self {
        self.source = Some(Arc::new(source));
        self
    }
}

impl Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.action {
            MessageAction::Sending => {
                write!(f, "Error while sending message.")?;
            }
            MessageAction::Receiving => {
                write!(f, "Error while receiving message.")?;
            }
        }

        match self.error_type {
            MessageErrorType::ConnectionClosed => write!(f, " Connection is closing/closed."),
            MessageErrorType::UnableToSerialize => write!(f, " Unable to serialize message."),
            MessageErrorType::UnableToDeserialze => write!(f, " Unable to deserialize message."),
            MessageErrorType::Websocket => write!(f, " Error with the websocket connection."),
        }
    }
}

impl std::error::Error for MessageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| &**source)
    }
}

#[derive(Debug, Clone)]
pub enum MessageAction {
    Sending,
    Receiving,
}

#[derive(Debug, Clone)]
pub enum MessageErrorType {
    ConnectionClosed,
    UnableToSerialize,
    UnableToDeserialze,
    Websocket,
}

#[cfg(feature = "frontend")]
pub mod frontend;

#[cfg(feature = "backend")]
pub mod backend;
