use std::fmt;
use std::sync::{Arc, Mutex, TryLockError, TryLockResult};

use crossbeam_queue::SegQueue;

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
pub trait EventSinkWriter<T>: Send + Sync + 'static {
    /// Writes a value to the associated sink.
    fn write(&self, event: T);
}

/// An event sink iterator that returns all events that were broadcast by
/// connected output ports.
///
/// Events are returned in first-in-first-out order. Note that even if the
/// iterator returns `None`, it may still produce more items in the future (in
/// other words, it is not a [`FusedIterator`](std::iter::FusedIterator)).
pub struct EventQueue<T> {
    queue: Arc<SegQueue<T>>,
}

impl<T> EventQueue<T> {
    /// Creates a new `EventStream`.
    pub fn new() -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
        }
    }
}

impl<T: Send + 'static> EventSink<T> for EventQueue<T> {
    type Writer = EventQueueWriter<T>;

    fn writer(&self) -> Self::Writer {
        EventQueueWriter {
            queue: self.queue.clone(),
        }
    }
}

impl<T> Iterator for EventQueue<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop()
    }
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Debug for EventQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventQueue").finish_non_exhaustive()
    }
}

/// A producer handle of an `EventStream`.
pub struct EventQueueWriter<T> {
    queue: Arc<SegQueue<T>>,
}

impl<T: Send + 'static> EventSinkWriter<T> for EventQueueWriter<T> {
    /// Pushes an event onto the queue.
    fn write(&self, event: T) {
        self.queue.push(event);
    }
}

impl<T> fmt::Debug for EventQueueWriter<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EventQueueWriter").finish_non_exhaustive()
    }
}

/// A single-value event sink that holds the last event that was broadcast by
/// any of the connected output ports.
pub struct EventSlot<T> {
    slot: Arc<Mutex<Option<T>>>,
}

impl<T> EventSlot<T> {
    /// Creates a new `EventSlot`.
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Take the last event, if any, leaving the slot empty.
    ///
    /// Note that even after the event is taken, it may become populated anew.
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

impl<T: Send + 'static> EventSink<T> for EventSlot<T> {
    type Writer = EventSlotWriter<T>;

    /// Returns a writer handle.
    fn writer(&self) -> EventSlotWriter<T> {
        EventSlotWriter {
            slot: self.slot.clone(),
        }
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
    slot: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> EventSinkWriter<T> for EventSlotWriter<T> {
    /// Write an event into the slot.
    fn write(&self, event: T) {
        // Why do we just use `try_lock` and abandon if the lock is taken? The
        // reason is that (i) the reader is never supposed to access the slot
        // when the simulation runs and (ii) as a rule the simulator does not
        // warrant fairness when concurrently writing to an input. Therefore, if
        // the mutex is already locked when this writer attempts to lock it, it
        // means another writer is concurrently writing an event, and that event
        // is just as legitimate as ours so there is not need to overwrite it.
        match self.slot.try_lock() {
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
