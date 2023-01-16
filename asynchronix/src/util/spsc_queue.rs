//! Single-producer single-consumer unbounded FIFO queue that stores values in
//! fixed-size memory segments.

#![allow(unused)]

use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::atomic::Ordering;

use crossbeam_utils::CachePadded;

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::sync::atomic::{AtomicBool, AtomicPtr};
use crate::loom_exports::sync::Arc;

/// The number of slots in a single segment.
const SEGMENT_LEN: usize = 32;

/// A slot containing a single value.
struct Slot<T> {
    has_value: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            has_value: AtomicBool::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

/// A memory segment containing `SEGMENT_LEN` slots.
struct Segment<T> {
    /// Address of the next segment.
    ///
    /// A null pointer means that the next segment is not allocated yet.
    next_segment: AtomicPtr<Segment<T>>,
    data: [Slot<T>; SEGMENT_LEN],
}

impl<T> Segment<T> {
    /// Allocates a new segment.
    fn allocate_new() -> NonNull<Self> {
        let segment = Self {
            next_segment: AtomicPtr::new(ptr::null_mut()),
            data: Default::default(),
        };

        // Safety: the pointer is non-null since it comes from a box.
        unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(segment))) }
    }
}

/// The head of the queue from which values are popped.
struct Head<T> {
    /// Pointer to the segment at the head of the queue.
    segment: NonNull<Segment<T>>,
    /// Index of the next value to be read.
    ///
    /// If the index is equal to the segment length, it is necessary to move to
    /// the next segment before the next value can be read.
    next_read_idx: usize,
}

/// The tail of the queue to which values are pushed.
struct Tail<T> {
    /// Pointer to the segment at the tail of the queue.
    segment: NonNull<Segment<T>>,
    /// Index of the next value to be written.
    ///
    /// If the index is equal to the segment length, a new segment must be
    /// allocated before a new value can be written.
    next_write_idx: usize,
}

/// A single-producer, single-consumer unbounded FIFO queue.
struct Queue<T> {
    head: CachePadded<UnsafeCell<Head<T>>>,
    tail: CachePadded<UnsafeCell<Tail<T>>>,
}

impl<T> Queue<T> {
    /// Creates a new queue.
    fn new() -> Self {
        let segment = Segment::allocate_new();

        let head = Head {
            segment,
            next_read_idx: 0,
        };
        let tail = Tail {
            segment,
            next_write_idx: 0,
        };

        Self {
            head: CachePadded::new(UnsafeCell::new(head)),
            tail: CachePadded::new(UnsafeCell::new(tail)),
        }
    }

    /// Pushes a new value.
    ///
    /// # Safety
    ///
    /// The method cannot be called from multiple threads concurrently.
    unsafe fn push(&self, value: T) {
        // Safety: this is the only thread accessing the tail.
        let tail = self.tail.with_mut(|p| &mut *p);

        // If the whole segment has been written, allocate a new segment.
        if tail.next_write_idx == SEGMENT_LEN {
            let old_segment = tail.segment;
            tail.segment = Segment::allocate_new();

            // Safety: the old segment is still allocated since the consumer
            // cannot deallocate it before `next_segment` is set to a non-null
            // value.
            old_segment
                .as_ref()
                .next_segment
                .store(tail.segment.as_ptr(), Ordering::Release);

            tail.next_write_idx = 0;
        }

        // Safety: the tail segment is allocated since the consumer cannot
        // deallocate it before `next_segment` is set to a non-null value.
        let data = &tail.segment.as_ref().data[tail.next_write_idx];

        // Safety: we have exclusive access to the slot value since the consumer
        // cannot access it before `has_value` is set to true.
        data.value.with_mut(|p| (*p).write(value));

        // Ordering: this Release store synchronizes with the Acquire load in
        // `pop` and ensures that the value is visible to the consumer once
        // `has_value` reads `true`.
        data.has_value.store(true, Ordering::Release);

        tail.next_write_idx += 1;
    }

    /// Pops a new value.
    ///
    /// # Safety
    ///
    /// The method cannot be called from multiple threads concurrently.
    unsafe fn pop(&self) -> Option<T> {
        // Safety: this is the only thread accessing the head.
        let head = self.head.with_mut(|p| &mut *p);

        // If the whole segment has been read, try to move to the next segment.
        if head.next_read_idx == SEGMENT_LEN {
            // Read the next segment or return `None` if it is not ready yet.
            //
            // Safety: the head segment is still allocated since we are the only
            // thread that can deallocate it.
            let next_segment = head.segment.as_ref().next_segment.load(Ordering::Acquire);
            let next_segment = NonNull::new(next_segment)?;

            // Deallocate the old segment.
            //
            // Safety: the pointer was initialized from a box and the segment is
            // still allocated since we are the only thread that can deallocate
            // it.
            let _ = Box::from_raw(head.segment.as_ptr());

            // Update the segment and the next index.
            head.segment = next_segment;
            head.next_read_idx = 0;
        }

        let data = &head.segment.as_ref().data[head.next_read_idx];

        // Ordering: this Acquire load synchronizes with the Release store in
        // `push` and ensures that the value is visible once `has_value` reads
        // `true`.
        if !data.has_value.load(Ordering::Acquire) {
            return None;
        }

        // Safety: since `has_value` is `true` then we have exclusive ownership
        // of the value and we know that it was initialized.
        let value = data.value.with(|p| (*p).assume_init_read());

        head.next_read_idx += 1;

        Some(value)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            // Drop all values.
            while self.pop().is_some() {}

            // All values have been dropped: the last segment can be freed.

            // Safety: this is the only thread accessing the head since both the
            // consumer and producer have been dropped.
            let head = self.head.with_mut(|p| &mut *p);

            // Safety: the pointer was initialized from a box and the segment is
            // still allocated since we are the only thread that can deallocate
            // it.
            let _ = Box::from_raw(head.segment.as_ptr());
        }
    }
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> UnwindSafe for Queue<T> {}
impl<T> RefUnwindSafe for Queue<T> {}

/// A handle to a single-producer, single-consumer queue that can push values.
pub(crate) struct Producer<T> {
    queue: Arc<Queue<T>>,
    _non_sync_phantom: PhantomData<Cell<()>>,
}
impl<T> Producer<T> {
    /// Pushes a value to the queue.
    pub(crate) fn push(&self, value: T) -> Result<(), PushError> {
        if Arc::strong_count(&self.queue) == 1 {
            return Err(PushError {});
        }

        unsafe { self.queue.push(value) };

        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
/// Error returned when a push failed due to the consumer being dropped.
pub(crate) struct PushError {}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending message into a closed mailbox")
    }
}

impl Error for PushError {}

/// A handle to a single-producer, single-consumer queue that can pop values.
pub(crate) struct Consumer<T> {
    queue: Arc<Queue<T>>,
    _non_sync_phantom: PhantomData<Cell<()>>,
}
impl<T> Consumer<T> {
    /// Pops a value from the queue.
    pub(crate) fn pop(&self) -> Option<T> {
        unsafe { self.queue.pop() }
    }
}

/// Creates the producer and consumer handles of a single-producer,
/// single-consumer queue.
pub(crate) fn spsc_queue<T>() -> (Producer<T>, Consumer<T>) {
    let queue = Arc::new(Queue::new());

    let producer = Producer {
        queue: queue.clone(),
        _non_sync_phantom: PhantomData,
    };
    let consumer = Consumer {
        queue,
        _non_sync_phantom: PhantomData,
    };

    (producer, consumer)
}

/// Loom tests.
#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use super::*;

    use std::thread;

    #[test]
    fn spsc_queue_basic() {
        const VALUE_COUNT: usize = if cfg!(miri) { 1000 } else { 100_000 };

        let (producer, consumer) = spsc_queue();

        let th = thread::spawn(move || {
            for i in 0..VALUE_COUNT {
                let value = loop {
                    if let Some(v) = consumer.pop() {
                        break v;
                    }
                };

                assert_eq!(value, i);
            }
        });

        for i in 0..VALUE_COUNT {
            producer.push(i).unwrap();
        }

        th.join().unwrap();
    }
}

/// Loom tests.
#[cfg(all(test, asynchronix_loom))]
mod tests {
    use super::*;

    use loom::model::Builder;
    use loom::thread;

    #[test]
    fn loom_spsc_queue_basic() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        const VALUE_COUNT: usize = 10;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (producer, consumer) = spsc_queue();

            let th = thread::spawn(move || {
                let mut value = 0;
                for _ in 0..VALUE_COUNT {
                    if let Some(v) = consumer.pop() {
                        assert_eq!(v, value);
                        value += 1;
                    }
                }
            });

            for i in 0..VALUE_COUNT {
                let _ = producer.push(i);
            }

            th.join().unwrap();
        });
    }

    #[test]
    fn loom_spsc_queue_new_segment() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        const VALUE_COUNT_BEFORE: usize = 5;
        const VALUE_COUNT_AFTER: usize = 5;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (producer, consumer) = spsc_queue();

            // Fill up the first segment except for the last `VALUE_COUNT_BEFORE` slots.
            for i in 0..(SEGMENT_LEN - VALUE_COUNT_BEFORE) {
                producer.push(i).unwrap();
                consumer.pop();
            }

            let th = thread::spawn(move || {
                let mut value = SEGMENT_LEN - VALUE_COUNT_BEFORE;
                for _ in (SEGMENT_LEN - VALUE_COUNT_BEFORE)..(SEGMENT_LEN + VALUE_COUNT_AFTER) {
                    if let Some(v) = consumer.pop() {
                        assert_eq!(v, value);
                        value += 1;
                    }
                }
            });

            for i in (SEGMENT_LEN - VALUE_COUNT_BEFORE)..(SEGMENT_LEN + VALUE_COUNT_AFTER) {
                let _ = producer.push(i);
            }

            th.join().unwrap();
        });
    }
}
