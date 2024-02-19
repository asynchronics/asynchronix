//! Model ports for event and query broadcasting.
//!
//! Models typically contain [`Output`] and/or [`Requestor`] ports, exposed as
//! public member variables. Output ports broadcast events to all connected
//! input ports, while requestor ports broadcast queries to, and retrieve
//! replies from, all connected replier ports.
//!
//! On the surface, output and requestor ports only differ in that sending a
//! query from a requestor port also returns an iterator over the replies from
//! all connected ports. Sending a query is more costly, however, because of the
//! need to wait until all connected models have processed the query. In
//! contrast, since events are buffered in the mailbox of the target model,
//! sending an event is a fire-and-forget operation. For this reason, output
//! ports should generally be preferred over requestor ports when possible.

use std::fmt;

mod broadcaster;
mod sender;

use crate::model::ports::sender::EventSinkSender;
use crate::model::{InputFn, Model, ReplierFn};
use crate::simulation::{Address, EventSink};

use broadcaster::{EventBroadcaster, QueryBroadcaster};

use self::sender::{InputSender, ReplierSender};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
/// Unique identifier for a connection between two ports.
pub struct LineId(u64);

/// An output port.
///
/// `Output` ports can be connected to input ports, i.e. to asynchronous model
/// methods that return no value. They broadcast events to all connected input
/// ports.
pub struct Output<T: Clone + Send + 'static> {
    broadcaster: EventBroadcaster<T>,
    next_line_id: u64,
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
        F: for<'a> InputFn<'a, M, T, S> + Copy,
        S: Send + 'static,
    {
        assert!(self.next_line_id != u64::MAX);
        let line_id = LineId(self.next_line_id);
        self.next_line_id += 1;
        let sender = Box::new(InputSender::new(input, address.into().0));
        self.broadcaster.add(sender, line_id);

        line_id
    }

    /// Adds a connection to an event sink such as an
    /// [`EventSlot`](crate::simulation::EventSlot) or
    /// [`EventQueue`](crate::simulation::EventQueue).
    pub fn connect_sink<S: EventSink<T>>(&mut self, sink: &S) -> LineId {
        assert!(self.next_line_id != u64::MAX);
        let line_id = LineId(self.next_line_id);
        self.next_line_id += 1;
        let sender = Box::new(EventSinkSender::new(sink.writer()));
        self.broadcaster.add(sender, line_id);

        line_id
    }

    /// Removes the connection specified by the `LineId` parameter.
    ///
    /// It is a logic error to specify a line identifier from another [`Output`]
    /// or [`Requestor`] instance and may result in the disconnection of an
    /// arbitrary endpoint.
    pub fn disconnect(&mut self, line_id: LineId) -> Result<(), LineError> {
        if self.broadcaster.remove(line_id) {
            Ok(())
        } else {
            Err(LineError {})
        }
    }

    /// Removes all connections.
    pub fn disconnect_all(&mut self) {
        self.broadcaster.clear();
    }

    /// Broadcasts an event to all connected input ports.
    pub async fn send(&mut self, arg: T) {
        self.broadcaster.broadcast(arg).await.unwrap();
    }
}

impl<T: Clone + Send + 'static> Default for Output<T> {
    fn default() -> Self {
        Self {
            broadcaster: EventBroadcaster::default(),
            next_line_id: 0,
        }
    }
}

impl<T: Clone + Send + 'static> fmt::Debug for Output<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Output ({} connected ports)", self.broadcaster.len())
    }
}

/// A requestor port.
///
/// `Requestor` ports can be connected to replier ports, i.e. to asynchronous
/// model methods that return a value. They broadcast queries to all connected
/// replier ports.
pub struct Requestor<T: Clone + Send + 'static, R: Send + 'static> {
    broadcaster: QueryBroadcaster<T, R>,
    next_line_id: u64,
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
        F: for<'a> ReplierFn<'a, M, T, R, S> + Copy,
        S: Send + 'static,
    {
        assert!(self.next_line_id != u64::MAX);
        let line_id = LineId(self.next_line_id);
        self.next_line_id += 1;
        let sender = Box::new(ReplierSender::new(replier, address.into().0));
        self.broadcaster.add(sender, line_id);

        line_id
    }

    /// Removes the connection specified by the `LineId` parameter.
    ///
    /// It is a logic error to specify a line identifier from another [`Output`]
    /// or [`Requestor`] instance and may result in the disconnection of an
    /// arbitrary endpoint.
    pub fn disconnect(&mut self, line_id: LineId) -> Result<(), LineError> {
        if self.broadcaster.remove(line_id) {
            Ok(())
        } else {
            Err(LineError {})
        }
    }

    /// Removes all connections.
    pub fn disconnect_all(&mut self) {
        self.broadcaster.clear();
    }

    /// Broadcasts a query to all connected replier ports.
    pub async fn send(&mut self, arg: T) -> impl Iterator<Item = R> + '_ {
        self.broadcaster.broadcast(arg).await.unwrap()
    }
}

impl<T: Clone + Send + 'static, R: Send + 'static> Default for Requestor<T, R> {
    fn default() -> Self {
        Self {
            broadcaster: QueryBroadcaster::default(),
            next_line_id: 0,
        }
    }
}

impl<T: Clone + Send + 'static, R: Send + 'static> fmt::Debug for Requestor<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Requestor ({} connected ports)", self.broadcaster.len())
    }
}

/// Error raised when the specified line cannot be found.
#[derive(Copy, Clone, Debug)]
pub struct LineError {}
