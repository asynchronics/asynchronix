use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, TryLockResult};

use super::{EventSink, EventSinkStream, EventSinkWriter};

/// The shared data of an `EventBuffer`.
struct Inner<T> {
    is_open: AtomicBool,
    slot: Mutex<Option<T>>,
}

/// An `EventSink` and `EventSinkStream` that only keeps the last event.
///
/// Once the value is read, the iterator will return `None` until a new value is
/// received. If the slot contains a value when a new value is received, the
/// previous value is overwritten.
pub struct EventSlot<T> {
    inner: Arc<Inner<T>>,
}

impl<T> EventSlot<T> {
    /// Creates an open `EventSlot`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                is_open: AtomicBool::new(true),
                slot: Mutex::new(None),
            }),
        }
    }

    /// Creates a closed `EventSlot`.
    pub fn new_closed() -> Self {
        Self {
            inner: Arc::new(Inner {
                is_open: AtomicBool::new(false),
                slot: Mutex::new(None),
            }),
        }
    }
}

impl<T: Send + 'static> EventSink<T> for EventSlot<T> {
    type Writer = EventSlotWriter<T>;

    /// Returns a writer handle.
    fn writer(&self) -> EventSlotWriter<T> {
        EventSlotWriter {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Iterator for EventSlot<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.slot.try_lock() {
            TryLockResult::Ok(mut v) => v.take(),
            TryLockResult::Err(TryLockError::WouldBlock) => None,
            TryLockResult::Err(TryLockError::Poisoned(_)) => panic!(),
        }
    }
}

impl<T: Send + 'static> EventSinkStream for EventSlot<T> {
    fn open(&mut self) {
        self.inner.is_open.store(true, Ordering::Relaxed);
    }
    fn close(&mut self) {
        self.inner.is_open.store(false, Ordering::Relaxed);
    }
}

impl<T> Default for EventSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Debug for EventSlot<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventSlot").finish_non_exhaustive()
    }
}

/// A writer handle of an `EventSlot`.
pub struct EventSlotWriter<T> {
    inner: Arc<Inner<T>>,
}

impl<T: Send + 'static> EventSinkWriter<T> for EventSlotWriter<T> {
    /// Write an event into the slot.
    fn write(&self, event: T) {
        // Ignore if the sink is closed.
        if !self.inner.is_open.load(Ordering::Relaxed) {
            return;
        }

        // Why do we just use `try_lock` and abandon if the lock is taken? The
        // reason is that (i) the reader is never supposed to access the slot
        // when the simulation runs and (ii) as a rule the simulator does not
        // warrant fairness when concurrently writing to an input. Therefore, if
        // the mutex is already locked when this writer attempts to lock it, it
        // means another writer is concurrently writing an event, and that event
        // is just as legitimate as ours so there is not need to overwrite it.
        match self.inner.slot.try_lock() {
            TryLockResult::Ok(mut v) => *v = Some(event),
            TryLockResult::Err(TryLockError::WouldBlock) => {}
            TryLockResult::Err(TryLockError::Poisoned(_)) => panic!(),
        }
    }
}

impl<T> fmt::Debug for EventSlotWriter<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventStreamWriter").finish_non_exhaustive()
    }
}
