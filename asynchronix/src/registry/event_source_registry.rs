use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use rmp_serde::decode::Error as RmpDecodeError;
use serde::de::DeserializeOwned;

use crate::ports::EventSource;
use crate::simulation::{Action, ActionKey};

/// A registry that holds all sources and sinks meant to be accessed through
/// remote procedure calls.
#[derive(Default)]
pub(crate) struct EventSourceRegistry(HashMap<String, Box<dyn EventSourceAny>>);

impl EventSourceRegistry {
    /// Adds an event source to the registry.
    ///
    /// If the specified name is already in use for another event source, the source
    /// provided as argument is returned in the error.
    pub(crate) fn add<T>(
        &mut self,
        source: EventSource<T>,
        name: impl Into<String>,
    ) -> Result<(), EventSource<T>>
    where
        T: DeserializeOwned + Clone + Send + 'static,
    {
        match self.0.entry(name.into()) {
            Entry::Vacant(s) => {
                s.insert(Box::new(source));

                Ok(())
            }
            Entry::Occupied(_) => Err(source),
        }
    }

    /// Returns a mutable reference to the specified event source if it is in
    /// the registry.
    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut dyn EventSourceAny> {
        self.0.get_mut(name).map(|s| s.as_mut())
    }
}

impl fmt::Debug for EventSourceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "EventSourceRegistry ({} sources)", self.0.len())
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
