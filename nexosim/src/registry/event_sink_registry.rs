use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;

use ciborium;
use serde::Serialize;

use crate::ports::EventSinkStream;

type SerializationError = ciborium::ser::Error<std::io::Error>;

/// A registry that holds all sources and sinks meant to be accessed through
/// remote procedure calls.
#[derive(Default)]
pub(crate) struct EventSinkRegistry(HashMap<String, Box<dyn EventSinkStreamAny>>);

impl EventSinkRegistry {
    /// Adds a sink to the registry.
    ///
    /// If the specified name is already in use for another sink, the sink
    /// provided as argument is returned in the error.
    pub(crate) fn add<S>(&mut self, sink: S, name: impl Into<String>) -> Result<(), S>
    where
        S: EventSinkStream + Send + 'static,
        S::Item: Serialize,
    {
        match self.0.entry(name.into()) {
            Entry::Vacant(s) => {
                s.insert(Box::new(sink));

                Ok(())
            }
            Entry::Occupied(_) => Err(sink),
        }
    }

    /// Returns a mutable reference to the specified sink if it is in the
    /// registry.
    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut dyn EventSinkStreamAny> {
        self.0.get_mut(name).map(|s| s.as_mut())
    }
}

impl fmt::Debug for EventSinkRegistry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "EventSinkRegistry ({} sinks)", self.0.len())
    }
}

/// A type-erased `EventSinkStream`.
pub(crate) trait EventSinkStreamAny: Send + 'static {
    /// Human-readable name of the event type, as returned by
    /// `any::type_name()`.
    fn event_type_name(&self) -> &'static str;

    /// Starts or resumes the collection of new events.
    fn open(&mut self);

    /// Pauses the collection of new events.
    fn close(&mut self);

    /// Encode and collect all events in a vector.
    fn collect(&mut self) -> Result<Vec<Vec<u8>>, SerializationError>;
}

impl<E> EventSinkStreamAny for E
where
    E: EventSinkStream + Send + 'static,
    E::Item: Serialize,
{
    fn event_type_name(&self) -> &'static str {
        std::any::type_name::<E::Item>()
    }

    fn open(&mut self) {
        self.open();
    }

    fn close(&mut self) {
        self.close();
    }

    fn collect(&mut self) -> Result<Vec<Vec<u8>>, SerializationError> {
        self.__try_fold(Vec::new(), |mut encoded_events, event| {
            let mut buffer = Vec::new();
            ciborium::into_writer(&event, &mut buffer).map(|_| {
                encoded_events.push(buffer);

                encoded_events
            })
        })
    }
}
