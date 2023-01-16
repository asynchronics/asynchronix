use std::fmt;
use std::sync::{Arc, Mutex, TryLockError, TryLockResult};

use crate::util::spsc_queue;

/// An iterator that returns all events that were broadcast by an output port.
///
/// Events are returned in first-in-first-out order. Note that even if the
/// iterator returns `None`, it may still produce more items after simulation
/// time is incremented.
pub struct EventStream<T> {
    consumer: spsc_queue::Consumer<T>,
}

impl<T> EventStream<T> {
    /// Creates a new `EventStream`.
    pub(crate) fn new(consumer: spsc_queue::Consumer<T>) -> Self {
        Self { consumer }
    }
}

impl<T> Iterator for EventStream<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.consumer.pop()
    }
}

impl<T> fmt::Debug for EventStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

/// A single-value slot that holds the last event that was broadcast by an
/// output port.
pub struct EventSlot<T> {
    slot: Arc<Mutex<Option<T>>>,
}

impl<T> EventSlot<T> {
    /// Creates a new `EventSlot`.
    pub(crate) fn new(slot: Arc<Mutex<Option<T>>>) -> Self {
        Self { slot }
    }

    /// Take the last event, if any, leaving the slot empty.
    ///
    /// Note that even after the event is taken, it may become populated anew
    /// after simulation time is incremented.
    pub fn take(&mut self) -> Option<T> {
        // We don't actually need to take self by mutable reference, but this
        // signature is probably less surprising for the user and more
        // consistent with `EventStream`. It also prevents multi-threaded
        // access, which would be likely to be misused.
        match self.slot.try_lock() {
            TryLockResult::Ok(mut v) => v.take(),
            TryLockResult::Err(TryLockError::WouldBlock) => None,
            TryLockResult::Err(TryLockError::Poisoned(_)) => panic!(),
        }
    }
}

impl<T> fmt::Debug for EventSlot<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventSlot").finish_non_exhaustive()
    }
}
