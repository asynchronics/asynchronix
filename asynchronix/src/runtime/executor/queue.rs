use std::fmt;
use std::iter::FusedIterator;
use std::marker::PhantomData;
use std::mem::{drop, MaybeUninit};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;

use cache_padded::CachePadded;

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::sync::atomic::{AtomicU32, AtomicU64};
use crate::loom_exports::{debug_or_loom_assert, debug_or_loom_assert_eq};

pub(super) use buffers::*;

mod buffers;
#[cfg(test)]
mod tests;

/// A double-ended FIFO work-stealing queue.
///
/// The general operation of the queue is based on tokio's worker queue, itself
/// based on the Go scheduler's worker queue.
///
/// The queue tracks its tail and head position within a ring buffer with
/// wrap-around integers, where the least significant bits specify the actual
/// buffer index. All positions have bit widths that are intentionally larger
/// than necessary for buffer indexing because:
/// - an extra bit is needed to disambiguate between empty and full buffers when
///   the start and end position of the buffer are equal,
/// - the worker head is also used as long-cycle counter to mitigate the risk of
///   ABA.
///
#[derive(Debug)]
struct Queue<T, B: Buffer<T>> {
    /// Positions of the head as seen by the worker (most significant bits) and
    /// as seen by a stealer (least significant bits).
    heads: CachePadded<AtomicU64>,

    /// Position of the tail.
    tail: CachePadded<AtomicU32>,

    /// Queue items.
    buffer: Box<B::Data>,

    /// Make the type !Send and !Sync by default.
    _phantom: PhantomData<UnsafeCell<T>>,
}

impl<T, B: Buffer<T>> Queue<T, B> {
    /// Read an item at the given position.
    ///
    /// The position is automatically mapped to a valid buffer index using a
    /// modulo operation.
    ///
    /// # Safety
    ///
    /// The item at the given position must have been initialized before and
    /// cannot have been moved out.
    ///
    /// The caller must guarantee that the item at this position cannot be
    /// written to or moved out concurrently.
    #[inline]
    unsafe fn read_at(&self, position: u32) -> T {
        let index = (position & B::MASK) as usize;
        (*self.buffer).as_ref()[index].with(|slot| slot.read().assume_init())
    }

    /// Write an item at the given position.
    ///
    /// The position is automatically mapped to a valid buffer index using a
    /// modulo operation.
    ///
    /// # Note
    ///
    /// If an item is already initialized but was not moved out yet, it will be
    /// leaked.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the item at this position cannot be read
    /// or written to concurrently.
    #[inline]
    unsafe fn write_at(&self, position: u32, item: T) {
        let index = (position & B::MASK) as usize;
        (*self.buffer).as_ref()[index].with_mut(|slot| slot.write(MaybeUninit::new(item)));
    }

    /// Attempt to book `N` items for stealing where `N` is specified by a
    /// closure which takes as argument the total count of available items.
    ///
    /// In case of success, the returned tuple contains the stealer head and an
    /// item count at least equal to 1, in this order.
    ///
    /// # Errors
    ///
    /// An error is returned in the following cases:
    /// 1) no item could be stolen, either because the queue is empty or because
    ///    `N` is 0,
    /// 2) a concurrent stealing operation is ongoing.
    ///
    /// # Safety
    ///
    /// This function is not strictly unsafe, but because it initiates the
    /// stealing operation by modifying the post-stealing head in
    /// `push_count_and_head` without ever updating the `head` atomic variable,
    /// its misuse can result in permanently blocking subsequent stealing
    /// operations.
    fn book_items<C>(&self, mut count_fn: C, max_count: u32) -> Result<(u32, u32), StealError>
    where
        C: FnMut(usize) -> usize,
    {
        let mut heads = self.heads.load(Acquire);

        loop {
            let (worker_head, stealer_head) = unpack_heads(heads);

            // Bail out if both heads differ because it means another stealing
            // operation is concurrently ongoing.
            if stealer_head != worker_head {
                return Err(StealError::Busy);
            }

            let tail = self.tail.load(Acquire);
            let item_count = tail.wrapping_sub(worker_head);

            // `item_count` is tested now because `count_fn` may expect
            // `item_count>0`.
            if item_count == 0 {
                return Err(StealError::Empty);
            }

            // Unwind safety: it is OK if `count_fn` panics because no state has
            // been modified yet.
            let count =
                (count_fn(item_count as usize).min(max_count as usize) as u32).min(item_count);

            // The special case `count_fn() == 0` must be tested specifically,
            // because if the compare-exchange succeeds with `count=0`, the new
            // worker head will be the same as the old one so other stealers
            // will not detect that stealing is currently ongoing and may try to
            // actually steal items and concurrently modify the position of the
            // heads.
            if count == 0 {
                return Err(StealError::Empty);
            }

            // Move the worker head only.
            let new_heads = pack_heads(worker_head.wrapping_add(count), stealer_head);

            // Attempt to book the slots. Only one stealer can succeed since
            // once this atomic is changed, the other thread will necessarily
            // observe a mismatch between the two heads.
            match self
                .heads
                .compare_exchange_weak(heads, new_heads, Acquire, Acquire)
            {
                Ok(_) => return Ok((stealer_head, count)),
                // We lost the race to a concurrent pop or steal operation, or
                // the CAS failed spuriously; try again.
                Err(h) => heads = h,
            }
        }
    }
}

impl<T, B: Buffer<T>> Drop for Queue<T, B> {
    fn drop(&mut self) {
        let worker_head = unpack_heads(self.heads.load(Relaxed)).0;
        let tail = self.tail.load(Relaxed);

        let count = tail.wrapping_sub(worker_head);

        for offset in 0..count {
            drop(unsafe { self.read_at(worker_head.wrapping_add(offset)) })
        }
    }
}

/// Handle for single-threaded FIFO push and pop operations.
#[derive(Debug)]
pub(super) struct Worker<T, B: Buffer<T>> {
    queue: Arc<Queue<T, B>>,
}

impl<T, B: Buffer<T>> Worker<T, B> {
    /// Creates a new queue and returns a `Worker` handle.
    pub(super) fn new() -> Self {
        let queue = Arc::new(Queue {
            heads: CachePadded::new(AtomicU64::new(0)),
            tail: CachePadded::new(AtomicU32::new(0)),
            buffer: B::allocate(),
            _phantom: PhantomData,
        });

        Worker { queue }
    }

    /// Creates a new `Stealer` handle associated to this `Worker`.
    ///
    /// An arbitrary number of `Stealer` handles can be created, either using
    /// this method or cloning an existing `Stealer` handle.
    pub(super) fn stealer(&self) -> Stealer<T, B> {
        Stealer {
            queue: self.queue.clone(),
        }
    }

    /// Returns the number of items that can be successfully pushed onto the
    /// queue.
    ///
    /// Note that that the spare capacity may be underestimated due to
    /// concurrent stealing operations.
    pub(super) fn spare_capacity(&self) -> usize {
        let capacity = <B as Buffer<T>>::CAPACITY;
        let stealer_head = unpack_heads(self.queue.heads.load(Relaxed)).1;
        let tail = self.queue.tail.load(Relaxed);

        // Aggregate count of available items (those which can be popped) and of
        // items currently being stolen.
        let len = tail.wrapping_sub(stealer_head);

        (capacity - len) as usize
    }

    /// Attempts to push one item at the tail of the queue.
    ///
    /// # Errors
    ///
    /// This will fail if the queue is full, in which case the item is returned
    /// as the error field.
    pub(super) fn push(&self, item: T) -> Result<(), T> {
        let stealer_head = unpack_heads(self.queue.heads.load(Acquire)).1;
        let tail = self.queue.tail.load(Relaxed);

        // Check that the buffer is not full.
        if tail.wrapping_sub(stealer_head) >= B::CAPACITY {
            return Err(item);
        }

        // Store the item.
        unsafe { self.queue.write_at(tail, item) };

        // Make the item visible by moving the tail.
        //
        // Ordering: the Release ordering ensures that the subsequent
        // acquisition of this atomic by a stealer will make the previous write
        // visible.
        self.queue.tail.store(tail.wrapping_add(1), Release);

        Ok(())
    }

    /// Attempts to push the content of an iterator at the tail of the queue.
    ///
    /// It is the responsibility of the caller to ensure that there is enough
    /// spare capacity to accommodate all iterator items, for instance by
    /// calling `[Worker::spare_capacity]` beforehand. Otherwise, the iterator
    /// is dropped while still holding the items in excess.
    pub(super) fn extend<I: IntoIterator<Item = T>>(&self, iter: I) {
        let stealer_head = unpack_heads(self.queue.heads.load(Acquire)).1;
        let mut tail = self.queue.tail.load(Relaxed);

        let max_tail = stealer_head.wrapping_add(B::CAPACITY);
        for item in iter {
            // Check whether the buffer is full.
            if tail == max_tail {
                break;
            }
            // Store the item.
            unsafe { self.queue.write_at(tail, item) };
            tail = tail.wrapping_add(1);
        }

        // Make the items visible by incrementing the push count.
        //
        // Ordering: the Release ordering ensures that the subsequent
        // acquisition of this atomic by a stealer will make the previous write
        // visible.
        self.queue.tail.store(tail, Release);
    }

    /// Attempts to pop one item from the head of the queue.
    ///
    /// This returns None if the queue is empty.
    pub(super) fn pop(&self) -> Option<T> {
        let mut heads = self.queue.heads.load(Acquire);

        let prev_worker_head = loop {
            let (worker_head, stealer_head) = unpack_heads(heads);
            let tail = self.queue.tail.load(Relaxed);

            // Check if the queue is empty.
            if tail == worker_head {
                return None;
            }

            // Move the worker head. The weird cast from `bool` to `u32` is to
            // steer the compiler towards branchless code.
            let next_heads = pack_heads(
                worker_head.wrapping_add(1),
                stealer_head.wrapping_add((stealer_head == worker_head) as u32),
            );

            // Attempt to book the items.
            let res = self
                .queue
                .heads
                .compare_exchange_weak(heads, next_heads, AcqRel, Acquire);

            match res {
                Ok(_) => break worker_head,
                // We lost the race to a stealer or the CAS failed spuriously; try again.
                Err(h) => heads = h,
            }
        };

        unsafe { Some(self.queue.read_at(prev_worker_head)) }
    }

    /// Returns an iterator that steals items from the head of the queue.
    ///
    /// The returned iterator steals up to `N` items, where `N` is specified by
    /// a closure which takes as argument the total count of items available for
    /// stealing. Upon success, the number of items ultimately stolen can be
    /// from 1 to `N`, depending on the number of available items.
    ///
    /// # Beware
    ///
    /// All items stolen by the iterator should be moved out as soon as
    /// possible, because until then or until the iterator is dropped, all
    /// concurrent stealing operations will fail with [`StealError::Busy`].
    ///
    /// # Leaking
    ///
    /// If the iterator is leaked before all stolen items have been moved out,
    /// subsequent stealing operations will permanently fail with
    /// [`StealError::Busy`].
    ///
    /// # Errors
    ///
    /// An error is returned in the following cases:
    /// 1) no item was stolen, either because the queue is empty or `N` is 0,
    /// 2) a concurrent stealing operation is ongoing.
    pub(super) fn drain<C>(&self, count_fn: C) -> Result<Drain<'_, T, B>, StealError>
    where
        C: FnMut(usize) -> usize,
    {
        let (head, count) = self.queue.book_items(count_fn, u32::MAX)?;

        Ok(Drain {
            queue: &self.queue,
            head,
            from_head: head,
            to_head: head.wrapping_add(count),
        })
    }
}

impl<T, B: Buffer<T>> Default for Worker<T, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, B: Buffer<T>> UnwindSafe for Worker<T, B> {}
impl<T, B: Buffer<T>> RefUnwindSafe for Worker<T, B> {}
unsafe impl<T: Send, B: Buffer<T>> Send for Worker<T, B> {}

/// A draining iterator for [`Worker<T, B>`].
///
/// This iterator is created by [`Worker::drain`]. See its documentation for
/// more.
#[derive(Debug)]
pub(super) struct Drain<'a, T, B: Buffer<T>> {
    queue: &'a Queue<T, B>,
    head: u32,
    from_head: u32,
    to_head: u32,
}

impl<'a, T, B: Buffer<T>> Iterator for Drain<'a, T, B> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.head == self.to_head {
            return None;
        }

        let item = Some(unsafe { self.queue.read_at(self.head) });

        self.head = self.head.wrapping_add(1);

        // We cannot rely on the caller to call `next` again after the last item
        // is yielded so the heads must be updated immediately when yielding the
        // last item.
        if self.head == self.to_head {
            // Signal that the stealing operation has completed.
            let mut heads = self.queue.heads.load(Relaxed);
            loop {
                let (worker_head, stealer_head) = unpack_heads(heads);

                debug_or_loom_assert_eq!(stealer_head, self.from_head);

                let res = self.queue.heads.compare_exchange_weak(
                    heads,
                    pack_heads(worker_head, worker_head),
                    AcqRel,
                    Acquire,
                );

                match res {
                    Ok(_) => break,
                    Err(h) => {
                        heads = h;
                    }
                }
            }
        }

        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let sz = self.to_head.wrapping_sub(self.head) as usize;

        (sz, Some(sz))
    }
}

impl<'a, T, B: Buffer<T>> ExactSizeIterator for Drain<'a, T, B> {}

impl<'a, T, B: Buffer<T>> FusedIterator for Drain<'a, T, B> {}

impl<'a, T, B: Buffer<T>> Drop for Drain<'a, T, B> {
    fn drop(&mut self) {
        // Drop all items and make sure the head is updated so that subsequent
        // stealing operations can succeed.
        for _item in self {}
    }
}

impl<'a, T, B: Buffer<T>> UnwindSafe for Drain<'a, T, B> {}
impl<'a, T, B: Buffer<T>> RefUnwindSafe for Drain<'a, T, B> {}
unsafe impl<'a, T: Send, B: Buffer<T>> Send for Drain<'a, T, B> {}
unsafe impl<'a, T: Send, B: Buffer<T>> Sync for Drain<'a, T, B> {}

/// Handle for multi-threaded stealing operations.
#[derive(Debug)]
pub(super) struct Stealer<T, B: Buffer<T>> {
    queue: Arc<Queue<T, B>>,
}

impl<T, B: Buffer<T>> Stealer<T, B> {
    /// Attempts to steal items from the head of the queue, returning one of
    /// them directly and moving the others to the tail of another queue.
    ///
    /// Up to `N` items are stolen (including the one returned directly), where
    /// `N` is specified by a closure which takes as argument the total count of
    /// items available for stealing. Upon success, one item is returned and
    /// from 0 to `N-1` items are moved to the destination queue, depending on
    /// the number of available items and the capacity of the destination queue.
    ///
    /// The returned item is the most recent one among the stolen items.
    ///
    /// # Errors
    ///
    /// An error is returned in the following cases:
    /// 1) no item was stolen, either because the queue is empty or `N` is 0,
    /// 2) a concurrent stealing operation is ongoing.
    ///
    /// Failure to transfer any item to the destination queue is not considered
    /// an error as long as one element could be returned directly. This can
    /// occur if the destination queue is full, if the source queue has only one
    /// item or if `N` is 1.
    pub(super) fn steal_and_pop<C, BDest>(
        &self,
        dest: &Worker<T, BDest>,
        count_fn: C,
    ) -> Result<T, StealError>
    where
        C: FnMut(usize) -> usize,
        BDest: Buffer<T>,
    {
        // Compute the free capacity of the destination queue.
        //
        // Ordering: see `Worker::push()` method.
        let dest_tail = dest.queue.tail.load(Relaxed);
        let dest_stealer_head = unpack_heads(dest.queue.heads.load(Acquire)).1;
        let dest_free_capacity = BDest::CAPACITY - dest_tail.wrapping_sub(dest_stealer_head);

        debug_or_loom_assert!(dest_free_capacity <= BDest::CAPACITY);

        let (stealer_head, count) = self.queue.book_items(count_fn, dest_free_capacity + 1)?;
        let transfer_count = count - 1;

        debug_or_loom_assert!(transfer_count <= dest_free_capacity);

        // Move all items but the last to the destination queue.
        for offset in 0..transfer_count {
            unsafe {
                let item = self.queue.read_at(stealer_head.wrapping_add(offset));
                dest.queue.write_at(dest_tail.wrapping_add(offset), item);
            }
        }

        // Read the last item.
        let last_item = unsafe {
            self.queue
                .read_at(stealer_head.wrapping_add(transfer_count))
        };

        // Make the moved items visible by updating the destination tail position.
        //
        // Ordering: see comments in the `push()` method.
        dest.queue
            .tail
            .store(dest_tail.wrapping_add(transfer_count), Release);

        // Signal that the stealing operation has completed.
        let mut heads = self.queue.heads.load(Relaxed);
        loop {
            let (worker_head, sh) = unpack_heads(heads);

            debug_or_loom_assert_eq!(stealer_head, sh);

            let res = self.queue.heads.compare_exchange_weak(
                heads,
                pack_heads(worker_head, worker_head),
                AcqRel,
                Acquire,
            );

            match res {
                Ok(_) => return Ok(last_item),
                Err(h) => {
                    heads = h;
                }
            }
        }
    }
}

impl<T, B: Buffer<T>> Clone for Stealer<T, B> {
    fn clone(&self) -> Self {
        Stealer {
            queue: self.queue.clone(),
        }
    }
}

impl<T, B: Buffer<T>> UnwindSafe for Stealer<T, B> {}
impl<T, B: Buffer<T>> RefUnwindSafe for Stealer<T, B> {}
unsafe impl<T: Send, B: Buffer<T>> Send for Stealer<T, B> {}
unsafe impl<T: Send, B: Buffer<T>> Sync for Stealer<T, B> {}

/// Error returned when stealing is unsuccessful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StealError {
    /// No item was stolen.
    Empty,
    /// Another concurrent stealing operation is ongoing.
    Busy,
}

impl fmt::Display for StealError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StealError::Empty => write!(f, "cannot steal from empty queue"),
            StealError::Busy => write!(f, "a concurrent steal operation is ongoing"),
        }
    }
}

#[inline(always)]
/// Extract the worker head and stealer head (in this order) from packed heads.
fn unpack_heads(heads: u64) -> (u32, u32) {
    ((heads >> u32::BITS) as u32, heads as u32)
}

#[inline(always)]
/// Insert a new stealer head into packed heads.
fn pack_heads(worker_head: u32, stealer_head: u32) -> u64 {
    ((worker_head as u64) << u32::BITS) | stealer_head as u64
}
