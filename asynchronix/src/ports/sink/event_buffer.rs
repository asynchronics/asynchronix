use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{EventSink, EventSinkStream, EventSinkWriter};

/// The shared data of an `EventBuffer`.
struct Inner<T> {
    capacity: usize,
    is_open: AtomicBool,
    buffer: Mutex<VecDeque<T>>,
}

/// An [`EventSink`] and [`EventSinkStream`] with a bounded size.
///
/// If the maximum capacity is exceeded, older events are overwritten. Events
/// are returned in first-in-first-out order. Note that even if the iterator
/// returns `None`, it may still produce more items in the future (in other
/// words, it is not a [`FusedIterator`](std::iter::FusedIterator)).
pub struct EventBuffer<T> {
    inner: Arc<Inner<T>>,
}

impl<T> EventBuffer<T> {
    /// Default capacity when constructed with `new`.
    pub const DEFAULT_CAPACITY: usize = 16;

    /// Creates an open `EventBuffer` with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// Creates a closed `EventBuffer` with the default capacity.
    pub fn new_closed() -> Self {
        Self::with_capacity_closed(Self::DEFAULT_CAPACITY)
    }

    /// Creates an open `EventBuffer` with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                capacity,
                is_open: AtomicBool::new(true),
                buffer: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Creates a closed `EventBuffer` with the specified capacity.
    pub fn with_capacity_closed(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                capacity,
                is_open: AtomicBool::new(false),
                buffer: Mutex::new(VecDeque::new()),
            }),
        }
    }
}

impl<T: Send + 'static> EventSink<T> for EventBuffer<T> {
    type Writer = EventBufferWriter<T>;

    fn writer(&self) -> Self::Writer {
        EventBufferWriter {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Iterator for EventBuffer<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.buffer.lock().unwrap().pop_front()
    }
}

impl<T: Send + 'static> EventSinkStream for EventBuffer<T> {
    fn open(&mut self) {
        self.inner.is_open.store(true, Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.inner.is_open.store(false, Ordering::Relaxed);
    }

    fn try_fold<B, F, E>(&mut self, init: B, f: F) -> Result<B, E>
    where
        Self: Sized,
        F: FnMut(B, Self::Item) -> Result<B, E>,
    {
        let mut inner = self.inner.buffer.lock().unwrap();
        let mut drain = inner.drain(..);

        drain.try_fold(init, f)
    }
}

impl<T> Default for EventBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Debug for EventBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventBuffer").finish_non_exhaustive()
    }
}

/// A producer handle of an `EventStream`.
pub struct EventBufferWriter<T> {
    inner: Arc<Inner<T>>,
}

impl<T: Send + 'static> EventSinkWriter<T> for EventBufferWriter<T> {
    /// Pushes an event onto the queue.
    fn write(&self, event: T) {
        if !self.inner.is_open.load(Ordering::Relaxed) {
            return;
        }

        let mut buffer = self.inner.buffer.lock().unwrap();
        if buffer.len() == self.inner.capacity {
            buffer.pop_front();
        }

        buffer.push_back(event);
    }
}

impl<T> fmt::Debug for EventBufferWriter<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventBufferWriter").finish_non_exhaustive()
    }
}
