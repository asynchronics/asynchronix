pub(crate) mod event_buffer;
pub(crate) mod event_slot;

/// A simulation endpoint that can receive events sent by model outputs.
///
/// An `EventSink` can be thought of as a self-standing input meant to
/// externally monitor the simulated system.
pub trait EventSink<T> {
    /// Writer handle to an event sink.
    type Writer: EventSinkWriter<T>;

    /// Returns the writer handle associated to this sink.
    fn writer(&self) -> Self::Writer;
}

/// A writer handle to an event sink.
pub trait EventSinkWriter<T>: Clone + Send + Sync + 'static {
    /// Writes a value to the associated sink.
    fn write(&self, event: T);
}

/// An iterator over collected events with the ability to pause and resume event
/// collection.
///
/// An `EventSinkStream` will typically be implemented on an `EventSink` for
/// which it will constitute a draining iterator.
pub trait EventSinkStream: Iterator {
    /// Starts or resumes the collection of new events.
    fn open(&mut self);

    /// Pauses the collection of new events.
    ///
    /// Events that were previously in the stream remain available.
    fn close(&mut self);

    /// This is a stop-gap method that serves the exact same purpose as
    /// `Iterator::try_fold` but is specialized for `Result` rather than the
    /// `Try` trait so it can be implemented on stable Rust.
    ///
    /// It makes it possible to provide a faster implementation when the event
    /// sink stream can be iterated over more rapidly than by repeatably calling
    /// `Iterator::next`, for instance if the implementation of the stream
    /// relies on a mutex that must be locked on each call.
    ///
    /// It is not publicly implementable because it may be removed at any time
    /// once the `Try` trait is stabilized, without regard for backward
    /// compatibility.
    #[doc(hidden)]
    #[allow(private_interfaces)]
    fn __try_fold<B, F, E>(&mut self, init: B, f: F) -> Result<B, E>
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> Result<B, E>,
    {
        Iterator::try_fold(self, init, f)
    }
}
