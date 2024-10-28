//! A bounded MPSC queue, based on Dmitry Vyukov's MPMC queue.
//!
//! The messages stored in the queue are async closures that can be called with
//! a `&mut M` argument and an empty `RecycleBox` to generate a boxed future.

use std::cmp;
use std::fmt;
use std::mem::{self, ManuallyDrop};
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::Ordering;

use crossbeam_utils::CachePadded;
use recycle_box::RecycleBox;

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::debug_or_loom_assert_eq;
use crate::loom_exports::sync::atomic::AtomicUsize;

/// A message borrowed from the queue.
///
/// The borrowed message should be dropped as soon as possible because its slot
/// in the queue cannot be re-used until then.
///
/// # Leaks
///
/// Leaking this borrow will eventually prevent more messages to be pushed to
/// the queue.
pub(super) struct MessageBorrow<'a, T: ?Sized> {
    queue: &'a Queue<T>,
    msg: ManuallyDrop<RecycleBox<T>>,
    index: usize,
    stamp: usize,
}

impl<'a, T: ?Sized> Deref for MessageBorrow<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.msg
    }
}

impl<'a, T: ?Sized> DerefMut for MessageBorrow<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.msg
    }
}

impl<'a, T: ?Sized> Drop for MessageBorrow<'a, T> {
    fn drop(&mut self) {
        let slot = &self.queue.buffer[self.index];

        // Safety: the content of the `ManuallyDrop` will not be accessed anymore.
        let recycle_box = RecycleBox::vacate(unsafe { ManuallyDrop::take(&mut self.msg) });

        // Give the box back to the queue.
        //
        // Safety: the slot can be safely accessed because it has not yet been
        // marked as empty.
        unsafe {
            slot.message
                .with_mut(|p| *p = MessageBox::Vacated(recycle_box));
        }

        // Mark the slot as empty.
        slot.stamp.store(self.stamp, Ordering::Release);
    }
}
impl<'a, M> fmt::Debug for MessageBorrow<'a, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageBorrow").finish_non_exhaustive()
    }
}

enum MessageBox<T: ?Sized> {
    Populated(RecycleBox<T>),
    Vacated(RecycleBox<()>),
    None,
}

/// A queue slot with a stamp and either a boxed messaged or an empty box.
struct Slot<T: ?Sized> {
    stamp: AtomicUsize,
    message: UnsafeCell<MessageBox<T>>,
}

/// A fast MPSC queue that stores its items in recyclable boxes.
///
/// The item may be unsized.
///
/// The enqueue position, dequeue position and the slot stamps are all stored as
/// `usize` and share the following layout:
///
/// ```text
///
/// | <- MSB                                LSB -> |
/// | Sequence count | flag (1 bit) | Buffer index |
///
/// ```
///
/// The purpose of the flag differs depending on the field:
///
/// - enqueue position: if set, the flag signals that the queue has been closed
///   by either the consumer or a producer,
/// - dequeue position: the flag is not used (always 0),
/// - slot stamp: the flag de-facto extends the mantissa of the buffer index,
///   which makes it in particular possible to support queues with a capacity of
///   1 without special-casing.
///
pub(super) struct Queue<T: ?Sized> {
    /// Buffer position of the slot to which the next closure will be written.
    ///
    /// The position stores the buffer index in the least significant bits and a
    /// sequence counter in the most significant bits.
    enqueue_pos: CachePadded<AtomicUsize>,

    /// Buffer position of the slot from which the next closure will be read.
    ///
    /// This is only ever mutated from a single thread but it must be stored in
    /// an atomic or an `UnsafeCell` since it is shared between the consumers
    /// and the producer. The reason it is shared is that the drop handler of
    /// the last `Inner` owner (which may be a producer) needs access to the
    /// dequeue position.
    dequeue_pos: CachePadded<AtomicUsize>,

    /// Buffer holding the closures and their stamps.
    buffer: Box<[Slot<T>]>,

    /// Bit mask covering both the buffer index and the 1-bit flag.
    right_mask: usize,

    /// Bit mask for the 1-bit flag, used as closed-channel flag in the enqueue
    /// position.
    closed_channel_mask: usize,
}

impl<T: ?Sized> Queue<T> {
    /// Creates a new `Inner`.
    pub(super) fn new(capacity: usize) -> Self {
        assert!(capacity >= 1, "the capacity must be 1 or greater");

        assert!(
            capacity <= (1 << (usize::BITS - 1)),
            "the capacity may not exceed {}",
            1usize << (usize::BITS - 1)
        );

        // Allocate a buffer initialized with linearly increasing stamps.
        let mut buffer = Vec::with_capacity(capacity);
        for i in 0..capacity {
            buffer.push(Slot {
                stamp: AtomicUsize::new(i),
                message: UnsafeCell::new(MessageBox::Vacated(RecycleBox::new(()))),
            });
        }

        let closed_channel_mask = capacity.next_power_of_two();
        let right_mask = (closed_channel_mask << 1).wrapping_sub(1);

        Queue {
            enqueue_pos: CachePadded::new(AtomicUsize::new(0)),
            dequeue_pos: CachePadded::new(AtomicUsize::new(0)),
            buffer: buffer.into(),
            right_mask,
            closed_channel_mask,
        }
    }

    /// Attempts to push an item in the queue.
    pub(super) fn push<F>(&self, msg_fn: F) -> Result<(), PushError<F>>
    where
        F: FnOnce(RecycleBox<()>) -> RecycleBox<T>,
    {
        let mut enqueue_pos = self.enqueue_pos.load(Ordering::Relaxed);

        loop {
            if enqueue_pos & self.closed_channel_mask != 0 {
                return Err(PushError::Closed);
            }

            let slot = &self.buffer[enqueue_pos & self.right_mask];
            let stamp = slot.stamp.load(Ordering::Acquire);

            let stamp_delta = stamp.wrapping_sub(enqueue_pos) as isize;

            match stamp_delta.cmp(&0) {
                cmp::Ordering::Equal => {
                    // The enqueue position matches the stamp: a push can be
                    // attempted.

                    // Try incrementing the enqueue position.
                    match self.enqueue_pos.compare_exchange_weak(
                        enqueue_pos,
                        self.next_queue_pos(enqueue_pos),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            // Write the closure into the slot and update the stamp.
                            unsafe {
                                slot.message.with_mut(|msg_fn_box| {
                                    let vacated_box =
                                        match mem::replace(&mut *msg_fn_box, MessageBox::None) {
                                            MessageBox::Vacated(b) => b,
                                            _ => unreachable!(),
                                        };

                                    *msg_fn_box = MessageBox::Populated(msg_fn(vacated_box))
                                });
                            };
                            slot.stamp.store(stamp.wrapping_add(1), Ordering::Release);

                            return Ok(());
                        }
                        Err(pos) => {
                            enqueue_pos = pos;
                        }
                    }
                }
                cmp::Ordering::Less => {
                    // The sequence count of the stamp is smaller than that of the
                    // enqueue position: the closure it contains has not been popped
                    // yet, so report a full queue.
                    return Err(PushError::Full(msg_fn));
                }
                cmp::Ordering::Greater => {
                    // The stamp is greater than the enqueue position: this means we
                    // raced with a concurrent producer which has already (i)
                    // incremented the enqueue position and (ii) written a closure to
                    // this slot. A retry is required.
                    enqueue_pos = self.enqueue_pos.load(Ordering::Relaxed);
                }
            }
        }
    }

    /// Attempts to pop an item from the queue.
    ///
    /// # Safety
    ///
    /// This method may not be called concurrently from multiple threads.
    pub(super) unsafe fn pop(&self) -> Result<MessageBorrow<'_, T>, PopError> {
        let dequeue_pos = self.dequeue_pos.load(Ordering::Relaxed);
        let index = dequeue_pos & self.right_mask;
        let slot = &self.buffer[index];
        let stamp = slot.stamp.load(Ordering::Acquire);

        if dequeue_pos != stamp {
            // The stamp is ahead of the dequeue position by 1 increment: the
            // closure can be popped.
            debug_or_loom_assert_eq!(stamp, dequeue_pos + 1);

            // Only this thread can modify the dequeue position so there is no
            // need to increment the position atomically with a `fetch_add`.
            self.dequeue_pos
                .store(self.next_queue_pos(dequeue_pos), Ordering::Relaxed);

            // Extract the closure from the slot and set the stamp to the value of
            // the dequeue position increased by one sequence increment.
            slot.message.with_mut(
                |msg_box| match mem::replace(&mut *msg_box, MessageBox::None) {
                    MessageBox::Populated(msg) => {
                        let borrow = MessageBorrow {
                            queue: self,
                            msg: ManuallyDrop::new(msg),
                            index,
                            stamp: stamp.wrapping_add(self.right_mask),
                        };

                        Ok(borrow)
                    }
                    _ => unreachable!(),
                },
            )
        } else {
            // Check whether the queue was closed. Even if the closed flag is
            // set and the slot is empty, there might still be a producer that
            // started a push before the channel was closed but has not yet
            // updated the stamp. For this reason, before returning
            // `PopError::Closed` it is necessary to check as well that the
            // enqueue position matches the dequeue position.
            //
            // Ordering: Relaxed ordering is enough since no closure will be read.
            if self.enqueue_pos.load(Ordering::Relaxed) == (dequeue_pos | self.closed_channel_mask)
            {
                Err(PopError::Closed)
            } else {
                Err(PopError::Empty)
            }
        }
    }

    /// Closes the queue.
    pub(super) fn close(&self) {
        // Set the closed-channel flag.
        //
        // Ordering: Relaxed ordering is enough here since neither the producers
        // nor the consumer rely on this flag for synchronizing reads and
        // writes.
        self.enqueue_pos
            .fetch_or(self.closed_channel_mask, Ordering::Relaxed);
    }

    /// Checks if the channel has been closed.
    ///
    /// Note that even if the channel is closed, some messages may still be
    /// present in the queue so further calls to `pop` may still succeed.
    pub(super) fn is_closed(&self) -> bool {
        // Read the closed-channel flag.
        //
        // Ordering: Relaxed ordering is enough here since this is merely an
        // informational function and cannot lead to any unsafety. If the load
        // is stale, the worse that can happen is that the queue is seen as open
        // when it is in fact already closed, which is OK since the caller must
        // anyway be resilient to the case where the channel closes right after
        // `is_closed` returns `false`.
        self.enqueue_pos.load(Ordering::Relaxed) & self.closed_channel_mask != 0
    }

    /// Returns the number of items in the queue.
    ///
    /// # Warning
    ///
    /// While this method is safe by Rust's standard, the returned result is
    /// only meaningful if it can be established than there are no concurrent
    /// `push` or `pop` operations. Otherwise, the returned value may neither
    /// reflect the current state nor the past state of the queue, and may be
    /// greater than the capacity of the queue.
    pub(super) fn len(&self) -> usize {
        let enqueue_pos = self.enqueue_pos.load(Ordering::Relaxed);
        let dequeue_pos = self.dequeue_pos.load(Ordering::Relaxed);
        let enqueue_idx = enqueue_pos & (self.right_mask >> 1);
        let dequeue_idx = dequeue_pos & (self.right_mask >> 1);

        // Establish whether the sequence numbers of the enqueue and dequeue
        // positions differ. If yes, it means the enqueue position has wrapped
        // around one more time so the difference between indices must be
        // increased by the buffer capacity.
        let carry_flag = (enqueue_pos & !self.right_mask) != (dequeue_pos & !self.right_mask);

        (enqueue_idx + (carry_flag as usize) * self.buffer.len()) - dequeue_idx
    }

    /// Increment the queue position, incrementing the sequence count as well if
    /// the index wraps to 0.
    ///
    /// Precondition when used with enqueue positions: the closed-channel flag
    /// should be cleared.
    #[inline]
    fn next_queue_pos(&self, queue_pos: usize) -> usize {
        debug_or_loom_assert_eq!(queue_pos & self.closed_channel_mask, 0);

        // The queue position cannot wrap around: in the worst case it will
        // overflow the flag bit.
        let new_queue_pos = queue_pos + 1;

        let new_index = new_queue_pos & self.right_mask;

        if new_index < self.buffer.len() {
            new_queue_pos
        } else {
            // The buffer index must wrap to 0 and the sequence count
            // must be incremented.
            let sequence_increment = self.right_mask + 1;
            let sequence_count = queue_pos & !self.right_mask;

            sequence_count.wrapping_add(sequence_increment)
        }
    }
}

unsafe impl<T: ?Sized + Send> Send for Queue<T> {}
unsafe impl<T: ?Sized + Send> Sync for Queue<T> {}

/// Error occurring when pushing into a queue is unsuccessful.
pub(super) enum PushError<F> {
    /// The queue is full.
    Full(F),
    /// The receiver has been dropped.
    Closed,
}

/// Error occurring when popping from a queue is unsuccessful.
#[derive(Debug)]
pub(super) enum PopError {
    /// The queue is empty.
    Empty,
    /// All senders have been dropped and the queue is empty.
    Closed,
}

/// Queue producer.
///
/// This is a safe queue producer proxy used for testing purposes only.
#[cfg(test)]
struct Producer<T: ?Sized> {
    inner: crate::loom_exports::sync::Arc<Queue<T>>,
}
#[cfg(test)]
impl<T: ?Sized> Producer<T> {
    /// Attempts to push an item into the queue.
    fn push<F>(&self, msg_fn: F) -> Result<(), PushError<F>>
    where
        F: FnOnce(RecycleBox<()>) -> RecycleBox<T>,
    {
        self.inner.push(msg_fn)
    }

    /// Closes the queue.
    pub(super) fn close(&self) {
        self.inner.close();
    }

    /// Checks if the queue is closed.
    #[cfg(not(asynchronix_loom))]
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}
#[cfg(test)]
impl<T> Clone for Producer<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Queue consumer.
///
/// This is a safe queue consumer proxy used for testing purposes only.
#[cfg(test)]
struct Consumer<T: ?Sized> {
    inner: crate::loom_exports::sync::Arc<Queue<T>>,
}
#[cfg(test)]
impl<T: ?Sized> Consumer<T> {
    /// Attempts to pop an item from the queue.
    fn pop(&mut self) -> Result<MessageBorrow<'_, T>, PopError> {
        // Safety: single-thread access is guaranteed since the consumer does
        // not implement `Clone` and `pop` requires exclusive ownership.
        unsafe { self.inner.pop() }
    }

    /// Closes the queue.
    fn close(&self) {
        self.inner.close();
    }

    /// Returns the number of items.
    #[cfg(not(asynchronix_loom))]
    fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
fn queue<T: ?Sized>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let inner = crate::loom_exports::sync::Arc::new(Queue::new(capacity));

    let producer = Producer {
        inner: inner.clone(),
    };
    let consumer = Consumer {
        inner: inner.clone(),
    };

    (producer, consumer)
}

/// Regular tests.
#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use super::*;

    use std::thread;

    #[test]
    fn queue_closed_by_sender() {
        let (p, mut c) = queue(3);

        assert!(matches!(c.pop(), Err(PopError::Empty)));

        assert!(matches!(p.push(|b| RecycleBox::recycle(b, 42)), Ok(_)));
        p.close();

        assert_eq!(*c.pop().unwrap(), 42);
        assert!(matches!(c.pop(), Err(PopError::Closed)));
    }

    #[test]
    fn queue_closed_by_consumer() {
        let (p, mut c) = queue(3);

        assert_eq!(p.is_closed(), false);
        assert!(matches!(p.push(|b| RecycleBox::recycle(b, 42)), Ok(_)));

        c.close();

        assert_eq!(p.is_closed(), true);
        assert!(matches!(
            p.push(|b| RecycleBox::recycle(b, 13)),
            Err(PushError::Closed)
        ));

        assert_eq!(*c.pop().unwrap(), 42);
        assert!(matches!(c.pop(), Err(PopError::Closed)));
    }

    fn queue_spsc(capacity: usize) {
        const COUNT: usize = if cfg!(miri) { 50 } else { 100_000 };

        let (p, mut c) = queue(capacity);

        let th_pop = thread::spawn(move || {
            for i in 0..COUNT {
                loop {
                    if let Ok(msg) = c.pop() {
                        assert_eq!(*msg, i);
                        break;
                    }
                }
            }
            assert!(c.pop().is_err());
        });

        let th_push = thread::spawn(move || {
            for i in 0..COUNT {
                while p.push(|b| RecycleBox::recycle(b, i)).is_err() {}
            }
        });

        th_pop.join().unwrap();
        th_push.join().unwrap();
    }

    #[test]
    fn queue_spsc_capacity_one() {
        queue_spsc(1);
    }
    #[test]
    fn queue_spsc_capacity_two() {
        queue_spsc(2);
    }
    #[test]
    fn queue_spsc_capacity_three() {
        queue_spsc(3);
    }

    fn queue_mpsc(capacity: usize) {
        const COUNT: usize = if cfg!(miri) { 20 } else { 25_000 };
        const PRODUCER_THREADS: usize = 4;

        let (p, mut c) = queue(capacity);
        let mut push_count = Vec::<usize>::new();
        push_count.resize_with(COUNT, Default::default);

        let th_push: Vec<_> = (0..PRODUCER_THREADS)
            .map(|_| {
                let p = p.clone();

                thread::spawn(move || {
                    for i in 0..COUNT {
                        while p.push(|b| RecycleBox::recycle(b, i)).is_err() {}
                    }
                })
            })
            .collect();

        for _ in 0..COUNT * PRODUCER_THREADS {
            let n = loop {
                if let Ok(x) = c.pop() {
                    break *x;
                }
            };

            push_count[n] += 1;
        }

        for c in push_count {
            assert_eq!(c, PRODUCER_THREADS);
        }

        for th in th_push {
            th.join().unwrap();
        }
    }

    #[test]
    fn queue_mpsc_capacity_one() {
        queue_mpsc(1);
    }
    #[test]
    fn queue_mpsc_capacity_two() {
        queue_mpsc(2);
    }
    #[test]
    fn queue_mpsc_capacity_three() {
        queue_mpsc(3);
    }

    #[test]
    fn queue_len() {
        let (p, mut c) = queue(4);

        let _ = p.push(|b| RecycleBox::recycle(b, 0));
        assert_eq!(c.len(), 1);
        let _ = p.push(|b| RecycleBox::recycle(b, 1));
        assert_eq!(c.len(), 2);
        let _ = c.pop();
        assert_eq!(c.len(), 1);
        let _ = p.push(|b| RecycleBox::recycle(b, 2));
        assert_eq!(c.len(), 2);
        let _ = p.push(|b| RecycleBox::recycle(b, 3));
        assert_eq!(c.len(), 3);
        let _ = c.pop();
        assert_eq!(c.len(), 2);
        let _ = p.push(|b| RecycleBox::recycle(b, 4));
        assert_eq!(c.len(), 3);
        let _ = c.pop();
        assert_eq!(c.len(), 2);
        let _ = p.push(|b| RecycleBox::recycle(b, 5));
        assert_eq!(c.len(), 3);
        let _ = p.push(|b| RecycleBox::recycle(b, 6));
        assert_eq!(c.len(), 4);
        let _ = c.pop();
        assert_eq!(c.len(), 3);
        let _ = p.push(|b| RecycleBox::recycle(b, 7));
        assert_eq!(c.len(), 4);
        let _ = c.pop();
        assert_eq!(c.len(), 3);
        let _ = p.push(|b| RecycleBox::recycle(b, 8));
        assert_eq!(c.len(), 4);
        let _ = c.pop();
        assert_eq!(c.len(), 3);
        let _ = p.push(|b| RecycleBox::recycle(b, 9));
        assert_eq!(c.len(), 4);
        let _ = c.pop();
        assert_eq!(c.len(), 3);
        let _ = c.pop();
        assert_eq!(c.len(), 2);
        let _ = c.pop();
        assert_eq!(c.len(), 1);
        let _ = c.pop();
        assert_eq!(c.len(), 0);
    }
}

/// Loom tests.
#[cfg(all(test, asynchronix_loom))]
mod tests {
    use super::*;

    use loom::model::Builder;
    use loom::sync::atomic::AtomicUsize;
    use loom::sync::Arc;
    use loom::thread;

    fn loom_queue_push_pop(
        max_push_per_thread: usize,
        producer_thread_count: usize,
        capacity: usize,
        preemption_bound: usize,
    ) {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(preemption_bound);
        }

        builder.check(move || {
            let (producer, mut consumer) = queue(capacity);

            let push_count = Arc::new(AtomicUsize::new(0));

            let producer_threads: Vec<_> = (0..producer_thread_count)
                .map(|_| {
                    let producer = producer.clone();
                    let push_count = push_count.clone();

                    thread::spawn(move || {
                        for i in 0..max_push_per_thread {
                            match producer.push(|b| RecycleBox::recycle(b, i)) {
                                Ok(()) => {}
                                Err(PushError::Full(_)) => {
                                    // A push can fail only if there is not enough capacity.
                                    assert!(capacity < max_push_per_thread * producer_thread_count);

                                    break;
                                }
                                Err(PushError::Closed) => panic!(),
                            }
                            push_count.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();

            let mut pop_count = 0;
            while consumer.pop().is_ok() {
                pop_count += 1;
            }

            for th in producer_threads {
                th.join().unwrap();
            }

            while consumer.pop().is_ok() {
                pop_count += 1;
            }

            assert_eq!(push_count.load(Ordering::Relaxed), pop_count);
        });
    }

    #[test]
    fn loom_queue_push_pop_overflow() {
        const DEFAULT_PREEMPTION_BOUND: usize = 5;
        loom_queue_push_pop(2, 2, 3, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_queue_push_pop_no_overflow() {
        const DEFAULT_PREEMPTION_BOUND: usize = 5;
        loom_queue_push_pop(2, 2, 5, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_queue_push_pop_capacity_power_of_two_overflow() {
        const DEFAULT_PREEMPTION_BOUND: usize = 5;
        loom_queue_push_pop(3, 2, 4, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_queue_push_pop_capacity_one_overflow() {
        const DEFAULT_PREEMPTION_BOUND: usize = 5;
        loom_queue_push_pop(2, 2, 1, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_queue_push_pop_capacity_power_of_two_no_overflow() {
        const DEFAULT_PREEMPTION_BOUND: usize = 5;
        loom_queue_push_pop(2, 2, 4, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_queue_push_pop_three_producers() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_queue_push_pop(2, 3, 3, DEFAULT_PREEMPTION_BOUND);
    }

    #[test]
    fn loom_queue_drop_items() {
        const CAPACITY: usize = 3;
        const PRODUCER_THREAD_COUNT: usize = 3;
        const DEFAULT_PREEMPTION_BOUND: usize = 4;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (producer, consumer) = queue(CAPACITY);
            let item = std::sync::Arc::new(()); // loom does not implement `strong_count()`

            let producer_threads: Vec<_> = (0..PRODUCER_THREAD_COUNT)
                .map(|_| {
                    thread::spawn({
                        let item = item.clone();
                        let producer = producer.clone();

                        move || {
                            assert!(matches!(
                                producer.push(|b| RecycleBox::recycle(b, item)),
                                Ok(_)
                            ));
                        }
                    })
                })
                .collect();

            for th in producer_threads {
                th.join().unwrap();
            }
            drop(producer);
            drop(consumer);

            assert_eq!(std::sync::Arc::strong_count(&item), 1);
        });
    }

    #[test]
    fn loom_queue_closed_by_producer() {
        const CAPACITY: usize = 3;
        const DEFAULT_PREEMPTION_BOUND: usize = 3;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (producer, mut consumer) = queue(CAPACITY);

            let th_push_close = thread::spawn({
                let producer = producer.clone();

                move || {
                    assert!(matches!(
                        producer.push(|b| RecycleBox::recycle(b, 7)),
                        Ok(_)
                    ));
                    producer.close();
                }
            });

            let th_try_push = thread::spawn({
                let producer = producer.clone();

                move || match producer.push(|b| RecycleBox::recycle(b, 13)) {
                    Ok(()) => true,
                    Err(PushError::Closed) => false,
                    _ => panic!(),
                }
            });

            let mut sum = 0;
            loop {
                match consumer.pop() {
                    Ok(n) => {
                        sum += *n;
                    }
                    Err(PopError::Closed) => break,
                    Err(PopError::Empty) => {}
                };
                thread::yield_now();
            }

            th_push_close.join().unwrap();
            let try_push_success = th_try_push.join().unwrap();
            if try_push_success {
                assert_eq!(sum, 7 + 13);
            } else {
                assert_eq!(sum, 7);
            }
        });
    }

    #[test]
    fn loom_queue_closed_by_consumer() {
        const CAPACITY: usize = 3;
        const DEFAULT_PREEMPTION_BOUND: usize = 3;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (producer, mut consumer) = queue(CAPACITY);

            let th_try_push1 = thread::spawn({
                let producer = producer.clone();

                move || match producer.push(|b| RecycleBox::recycle(b, 7)) {
                    Ok(()) => true,
                    Err(PushError::Closed) => false,
                    _ => panic!(),
                }
            });

            let th_try_push2 = thread::spawn({
                let producer = producer.clone();

                move || match producer.push(|b| RecycleBox::recycle(b, 13)) {
                    Ok(()) => true,
                    Err(PushError::Closed) => false,
                    _ => panic!(),
                }
            });

            let mut sum = 0;
            consumer.close();

            loop {
                match consumer.pop() {
                    Ok(n) => {
                        sum += *n;
                    }
                    Err(PopError::Closed) => break,
                    Err(PopError::Empty) => {}
                };
                thread::yield_now();
            }

            let try_push1_success = th_try_push1.join().unwrap();
            let try_push2_success = th_try_push2.join().unwrap();
            match (try_push1_success, try_push2_success) {
                (true, true) => assert_eq!(sum, 7 + 13),
                (true, false) => assert_eq!(sum, 7),
                (false, true) => assert_eq!(sum, 13),
                (false, false) => {}
            }
        });
    }
}
