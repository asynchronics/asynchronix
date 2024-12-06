//! Connector combinators.
//!
//! This module contains combinator types useful for simulation bench assembly.
//!

use nexosim::model::Model;
use nexosim::ports::{Output, Requestor};

/// A replier adaptor.
///
/// `ReplierAdaptor` generic model is aimed to connect pair of input/output
/// ports to a replier ports.
///
/// Model input is propagated to all the connected replier ports and their
/// answers are written to the model output.
pub struct ReplierAdaptor<T, R>
where
    T: Clone + Send + 'static,
    R: Clone + Send + 'static,
{
    /// Requestor port to be connected to replier port.
    pub requestor: Requestor<T, R>,

    /// Output port to be connected to input port.
    pub output: Output<R>,
}

impl<T, R> ReplierAdaptor<T, R>
where
    T: Clone + Send + 'static,
    R: Clone + Send + 'static,
{
    /// Creates a `ReplierAdaptor` model.
    pub fn new() -> Self {
        Self {
            requestor: Requestor::new(),
            output: Output::new(),
        }
    }

    /// Input port.
    pub async fn input(&mut self, data: T) {
        for res in self.requestor.send(data).await {
            self.output.send(res).await;
        }
    }
}

impl<T, R> Model for ReplierAdaptor<T, R>
where
    T: Clone + Send + 'static,
    R: Clone + Send + 'static,
{
}
