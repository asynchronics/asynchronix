mod broadcaster;
mod sender;

use std::fmt;

use crate::model::Model;
use crate::ports::{EventSink, LineError, LineId};
use crate::ports::{InputFn, ReplierFn};
use crate::simulation::Address;
use crate::util::cached_rw_lock::CachedRwLock;

use broadcaster::{EventBroadcaster, QueryBroadcaster};

use self::sender::{EventSinkSender, InputSender, ReplierSender};

/// An output port.
///
/// `Output` ports can be connected to input ports, i.e. to asynchronous model
/// methods that return no value. They broadcast events to all connected input
/// ports.
///
/// When an `Output` is cloned, the information on connected ports remains
/// shared and therefore all clones use and modify the same list of connected
/// ports.
#[derive(Clone)]
pub struct Output<T: Clone + Send + 'static> {
    broadcaster: CachedRwLock<EventBroadcaster<T>>,
}

impl<T: Clone + Send + 'static> Output<T> {
    /// Creates a new, disconnected `Output` port.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a connection to an input port of the model specified by the
    /// address.
    ///
    /// The input port must be an asynchronous method of a model of type `M`
    /// taking as argument a value of type `T` plus, optionally, a scheduler
    /// reference.
    pub fn connect<M, F, S>(&mut self, input: F, address: impl Into<Address<M>>) -> LineId
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        S: Send + 'static,
    {
        let sender = Box::new(InputSender::new(input, address.into().0));
        self.broadcaster.write().unwrap().add(sender)
    }

    /// Adds a connection to an event sink such as an
    /// [`EventSlot`](crate::ports::EventSlot) or
    /// [`EventBuffer`](crate::ports::EventBuffer).
    pub fn connect_sink<S: EventSink<T>>(&mut self, sink: &S) -> LineId {
        let sender = Box::new(EventSinkSender::new(sink.writer()));
        self.broadcaster.write().unwrap().add(sender)
    }

    /// Removes the connection specified by the `LineId` parameter.
    ///
    /// It is a logic error to specify a line identifier from another
    /// [`Output`], [`Requestor`], [`EventSource`](crate::ports::EventSource) or
    /// [`QuerySource`](crate::ports::QuerySource) instance and may result in
    /// the disconnection of an arbitrary endpoint.
    pub fn disconnect(&mut self, line_id: LineId) -> Result<(), LineError> {
        if self.broadcaster.write().unwrap().remove(line_id) {
            Ok(())
        } else {
            Err(LineError {})
        }
    }

    /// Removes all connections.
    pub fn disconnect_all(&mut self) {
        self.broadcaster.write().unwrap().clear();
    }

    /// Broadcasts an event to all connected input ports.
    pub async fn send(&mut self, arg: T) {
        let broadcaster = self.broadcaster.write_scratchpad().unwrap();
        broadcaster.broadcast(arg).await.unwrap();
    }
}

impl<T: Clone + Send + 'static> Default for Output<T> {
    fn default() -> Self {
        Self {
            broadcaster: CachedRwLock::new(EventBroadcaster::default()),
        }
    }
}

impl<T: Clone + Send + 'static> fmt::Debug for Output<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Output ({} connected ports)",
            self.broadcaster.read_unsync().len()
        )
    }
}

/// A requestor port.
///
/// `Requestor` ports can be connected to replier ports, i.e. to asynchronous
/// model methods that return a value. They broadcast queries to all connected
/// replier ports.
///
/// When a `Requestor` is cloned, the information on connected ports remains
/// shared and therefore all clones use and modify the same list of connected
/// ports.
pub struct Requestor<T: Clone + Send + 'static, R: Send + 'static> {
    broadcaster: CachedRwLock<QueryBroadcaster<T, R>>,
}

impl<T: Clone + Send + 'static, R: Send + 'static> Requestor<T, R> {
    /// Creates a new, disconnected `Requestor` port.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a connection to a replier port of the model specified by the
    /// address.
    ///
    /// The replier port must be an asynchronous method of a model of type `M`
    /// returning a value of type `R` and taking as argument a value of type `T`
    /// plus, optionally, a scheduler reference.
    pub fn connect<M, F, S>(&mut self, replier: F, address: impl Into<Address<M>>) -> LineId
    where
        M: Model,
        F: for<'a> ReplierFn<'a, M, T, R, S> + Clone,
        S: Send + 'static,
    {
        let sender = Box::new(ReplierSender::new(replier, address.into().0));
        self.broadcaster.write().unwrap().add(sender)
    }

    /// Removes the connection specified by the `LineId` parameter.
    ///
    /// It is a logic error to specify a line identifier from another
    /// [`Requestor`], [`Output`], [`EventSource`](crate::ports::EventSource) or
    /// [`QuerySource`](crate::ports::QuerySource) instance and may result in
    /// the disconnection of an arbitrary endpoint.
    pub fn disconnect(&mut self, line_id: LineId) -> Result<(), LineError> {
        if self.broadcaster.write().unwrap().remove(line_id) {
            Ok(())
        } else {
            Err(LineError {})
        }
    }

    /// Removes all connections.
    pub fn disconnect_all(&mut self) {
        self.broadcaster.write().unwrap().clear();
    }

    /// Broadcasts a query to all connected replier ports.
    pub async fn send(&mut self, arg: T) -> impl Iterator<Item = R> + '_ {
        self.broadcaster
            .write_scratchpad()
            .unwrap()
            .broadcast(arg)
            .await
            .unwrap()
    }
}

impl<T: Clone + Send + 'static, R: Send + 'static> Default for Requestor<T, R> {
    fn default() -> Self {
        Self {
            broadcaster: CachedRwLock::new(QueryBroadcaster::default()),
        }
    }
}

impl<T: Clone + Send + 'static, R: Send + 'static> fmt::Debug for Requestor<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Requestor ({} connected ports)",
            self.broadcaster.read_unsync().len()
        )
    }
}
