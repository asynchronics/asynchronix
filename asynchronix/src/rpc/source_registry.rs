use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use rmp_serde::decode::Error as RmpDecodeError;
use rmp_serde::encode::Error as RmpEncodeError;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::ports::{EventSource, QuerySource, ReplyReceiver};
use crate::simulation::{Action, ActionKey};

/// A registry that holds all sources and sinks meant to be accessed through
/// remote procedure calls.
#[derive(Default)]
pub struct SourceRegistry {
    event_sources: HashMap<String, Box<dyn EventSourceAny>>,
    query_sources: HashMap<String, Box<dyn QuerySourceAny>>,
}

impl SourceRegistry {
    /// Creates an empty `EndpointRegistry`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an event source to the registry.
    ///
    /// If the specified name is already in use for another event source, the source
    /// provided as argument is returned in the error.
    pub fn add_event_source<T>(
        &mut self,
        source: EventSource<T>,
        name: impl Into<String>,
    ) -> Result<(), EventSource<T>>
    where
        T: DeserializeOwned + Clone + Send + 'static,
    {
        match self.event_sources.entry(name.into()) {
            Entry::Vacant(s) => {
                s.insert(Box::new(source));

                Ok(())
            }
            Entry::Occupied(_) => Err(source),
        }
    }

    /// Returns a mutable reference to the specified event source if it is in
    /// the registry.
    pub(crate) fn get_event_source_mut(&mut self, name: &str) -> Option<&mut dyn EventSourceAny> {
        self.event_sources.get_mut(name).map(|s| s.as_mut())
    }

    /// Adds a query source to the registry.
    ///
    /// If the specified name is already in use for another query source, the
    /// source provided as argument is returned in the error.
    pub fn add_query_source<T, R>(
        &mut self,
        source: QuerySource<T, R>,
        name: impl Into<String>,
    ) -> Result<(), QuerySource<T, R>>
    where
        T: DeserializeOwned + Clone + Send + 'static,
        R: Serialize + Send + 'static,
    {
        match self.query_sources.entry(name.into()) {
            Entry::Vacant(s) => {
                s.insert(Box::new(source));

                Ok(())
            }
            Entry::Occupied(_) => Err(source),
        }
    }

    /// Returns a mutable reference to the specified query source if it is in
    /// the registry.
    pub(crate) fn get_query_source_mut(&mut self, name: &str) -> Option<&mut dyn QuerySourceAny> {
        self.query_sources.get_mut(name).map(|s| s.as_mut())
    }
}

impl fmt::Debug for SourceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "SourceRegistry ({} event sources, {} query sources)",
            self.event_sources.len(),
            self.query_sources.len()
        )
    }
}

/// A type-erased `EventSource` that operates on MessagePack-encoded serialized
/// events.
pub(crate) trait EventSourceAny: Send + 'static {
    /// Returns an action which, when processed, broadcasts an event to all
    /// connected input ports.
    ///
    /// The argument is expected to conform to the serde MessagePack encoding.
    fn event(&mut self, msgpack_arg: &[u8]) -> Result<Action, RmpDecodeError>;

    /// Returns a cancellable action and a cancellation key; when processed, the
    /// action broadcasts an event to all connected input ports.
    ///
    /// The argument is expected to conform to the serde MessagePack encoding.
    fn keyed_event(&mut self, msgpack_arg: &[u8]) -> Result<(Action, ActionKey), RmpDecodeError>;

    /// Returns a periodically recurring action which, when processed,
    /// broadcasts an event to all connected input ports.
    ///
    /// The argument is expected to conform to the serde MessagePack encoding.
    fn periodic_event(
        &mut self,
        period: Duration,
        msgpack_arg: &[u8],
    ) -> Result<Action, RmpDecodeError>;

    /// Returns a cancellable, periodically recurring action and a cancellation
    /// key; when processed, the action broadcasts an event to all connected
    /// input ports.
    ///
    /// The argument is expected to conform to the serde MessagePack encoding.
    fn keyed_periodic_event(
        &mut self,
        period: Duration,
        msgpack_arg: &[u8],
    ) -> Result<(Action, ActionKey), RmpDecodeError>;

    /// Human-readable name of the event type, as returned by
    /// `any::type_name()`.
    fn event_type_name(&self) -> &'static str;
}

impl<T> EventSourceAny for EventSource<T>
where
    T: DeserializeOwned + Clone + Send + 'static,
{
    fn event(&mut self, msgpack_arg: &[u8]) -> Result<Action, RmpDecodeError> {
        rmp_serde::from_read(msgpack_arg).map(|arg| self.event(arg))
    }
    fn keyed_event(&mut self, msgpack_arg: &[u8]) -> Result<(Action, ActionKey), RmpDecodeError> {
        rmp_serde::from_read(msgpack_arg).map(|arg| self.keyed_event(arg))
    }
    fn periodic_event(
        &mut self,
        period: Duration,
        msgpack_arg: &[u8],
    ) -> Result<Action, RmpDecodeError> {
        rmp_serde::from_read(msgpack_arg).map(|arg| self.periodic_event(period, arg))
    }
    fn keyed_periodic_event(
        &mut self,
        period: Duration,
        msgpack_arg: &[u8],
    ) -> Result<(Action, ActionKey), RmpDecodeError> {
        rmp_serde::from_read(msgpack_arg).map(|arg| self.keyed_periodic_event(period, arg))
    }
    fn event_type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

/// A type-erased `QuerySource` that operates on MessagePack-encoded serialized
/// queries and returns MessagePack-encoded replies.
pub(crate) trait QuerySourceAny: Send + 'static {
    /// Returns an action which, when processed, broadcasts a query to all
    /// connected replier ports.
    ///
    ///
    /// The argument is expected to conform to the serde MessagePack encoding.
    fn query(
        &mut self,
        msgpack_arg: &[u8],
    ) -> Result<(Action, Box<dyn ReplyReceiverAny>), RmpDecodeError>;

    /// Human-readable name of the request type, as returned by
    /// `any::type_name()`.
    fn request_type_name(&self) -> &'static str;

    /// Human-readable name of the reply type, as returned by
    /// `any::type_name()`.
    fn reply_type_name(&self) -> &'static str;
}

impl<T, R> QuerySourceAny for QuerySource<T, R>
where
    T: DeserializeOwned + Clone + Send + 'static,
    R: Serialize + Send + 'static,
{
    fn query(
        &mut self,
        msgpack_arg: &[u8],
    ) -> Result<(Action, Box<dyn ReplyReceiverAny>), RmpDecodeError> {
        rmp_serde::from_read(msgpack_arg).map(|arg| {
            let (action, reply_recv) = self.query(arg);
            let reply_recv: Box<dyn ReplyReceiverAny> = Box::new(reply_recv);

            (action, reply_recv)
        })
    }

    fn request_type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }

    fn reply_type_name(&self) -> &'static str {
        std::any::type_name::<R>()
    }
}

/// A type-erased `ReplyReceiver` that returns MessagePack-encoded replies..
pub(crate) trait ReplyReceiverAny {
    /// Take the replies, if any, encode them and collect them in a vector.
    fn take_collect(&mut self) -> Option<Result<Vec<Vec<u8>>, RmpEncodeError>>;
}

impl<R: Serialize + 'static> ReplyReceiverAny for ReplyReceiver<R> {
    fn take_collect(&mut self) -> Option<Result<Vec<Vec<u8>>, RmpEncodeError>> {
        let replies = self.take()?;

        let encoded_replies = (move || {
            let mut encoded_replies = Vec::new();
            for reply in replies {
                let encoded_reply = rmp_serde::to_vec_named(&reply)?;
                encoded_replies.push(encoded_reply);
            }

            Ok(encoded_replies)
        })();

        Some(encoded_replies)
    }
}
