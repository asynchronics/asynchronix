use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::{mem, vec};

/// An unfair injector queue which stores batches of tasks in bounded-size
/// buckets.
///
/// This is a simple but effective unfair injector design which, despite being
/// based on a mutex-protected `Vec`, ensures low contention and low latency in
/// most realistic cases.
///
/// This is achieved by enabling the worker to push and pop batches of tasks
/// readily stored in buckets. Since only the handles to the buckets are moved
/// to and from the injector, pushing and popping a bucket is very fast and the
/// lock is therefore only held for a very short time.
///
/// Also, since tasks in a bucket are memory-contiguous, they can be efficiently
/// copied to and from worker queues. The use of buckets also keeps the size of
/// the injector queue small (its size is the number of buckets) so
/// re-allocation is rare and fast.
///
/// As an additional optimization, an `is_empty` atomic flag allows workers
/// seeking for tasks to skip taking the lock if the queue is likely to be
/// empty.
///
/// The queue is not strictly LIFO. While buckets are indeed pushed and popped
/// in LIFO order, individual tasks are stored in a bucket at the front of the
/// queue and this bucket is only moved to the back of the queue when full.
#[derive(Debug)]
pub(crate) struct Injector<T, const BUCKET_CAPACITY: usize> {
    inner: Mutex<Vec<Bucket<T, BUCKET_CAPACITY>>>,
    is_empty: AtomicBool,
}

impl<T, const BUCKET_CAPACITY: usize> Injector<T, BUCKET_CAPACITY> {
    /// Creates an empty injector queue.
    ///
    /// # Panic
    ///
    /// Panics if the capacity is 0.
    pub(crate) const fn new() -> Self {
        assert!(BUCKET_CAPACITY >= 1);

        Self {
            inner: Mutex::new(Vec::new()),
            is_empty: AtomicBool::new(true),
        }
    }

    /// Inserts a task.
    ///
    /// The task is inserted in a bucket at the front of the queue. Once this
    /// bucket is full, it is moved to the back of the queue.
    pub(crate) fn insert_task(&self, task: T) {
        let mut inner = self.inner.lock().unwrap();

        // Try to push the task onto the first bucket if it has enough capacity left.
        if let Some(bucket) = inner.first_mut() {
            if let Err(task) = bucket.push(task) {
                // The bucket is full: move it to the back of the vector and
                // replace it with a newly created bucket that contains the
                // task.
                let mut new_bucket = Bucket::new();
                let _ = new_bucket.push(task); // this cannot fail provided the capacity is >=1

                let full_bucket = mem::replace(bucket, new_bucket);
                inner.push(full_bucket);
            }

            return;
        }

        // The queue is empty: create a new bucket.
        let mut new_bucket = Bucket::new();
        let _ = new_bucket.push(task); // this cannot fail provided the capacity is >=1

        inner.push(new_bucket);

        // Ordering: this flag is only used as a hint so Relaxed ordering is
        // sufficient.
        self.is_empty.store(false, Ordering::Relaxed);
    }

    /// Appends a bucket to the back of the queue.
    pub(crate) fn push_bucket(&self, bucket: Bucket<T, BUCKET_CAPACITY>) {
        let mut inner = self.inner.lock().unwrap();

        let was_empty = inner.is_empty();
        inner.push(bucket);

        // If the queue was empty before, update the flag.
        if was_empty {
            // Ordering: this flag is only used as a hint so Relaxed ordering is
            // sufficient.
            self.is_empty.store(false, Ordering::Relaxed);
        }
    }

    /// Takes the bucket at the back of the queue, if any.
    ///
    /// Note that this can spuriously return `None` even though the queue is
    /// populated, unless a happens-before relationship exists between the
    /// thread that populated the queue and the thread calling this method (this
    /// is obviously the case if they are the same thread).
    ///
    /// This is not an issue in practice because it cannot lead to executor
    /// deadlock. Indeed, if the last task/bucket was inserted by a worker
    /// thread, this worker thread will always see that the injector queue is
    /// populated (unless the bucket was already popped) so if all workers exit,
    /// then all tasks they have re-injected will necessarily have been
    /// processed. Likewise, if the last task/bucket was inserted by the main
    /// executor thread before `Executor::run()` is called, the synchronization
    /// established when the executor unparks worker threads ensures that the
    /// task is visible to all unparked workers.
    pub(crate) fn pop_bucket(&self) -> Option<Bucket<T, BUCKET_CAPACITY>> {
        // Ordering: this flag is only used as a hint so Relaxed ordering is
        // sufficient.
        if self.is_empty.load(Ordering::Relaxed) {
            return None;
        }

        let mut inner = self.inner.lock().unwrap();

        let bucket = inner.pop();

        if inner.is_empty() {
            // Ordering: this flag is only used as a hint so Relaxed ordering is
            // sufficient.
            self.is_empty.store(true, Ordering::Relaxed);
        }

        bucket
    }

    /// Checks whether the queue is empty.
    ///
    /// Note that this can spuriously return `true` even though the queue is
    /// populated, unless a happens-before relationship exists between the
    /// thread that populated the queue and the thread calling this method (this
    /// is obviously the case if they are the same thread).
    pub(crate) fn is_empty(&self) -> bool {
        self.is_empty.load(Ordering::Relaxed)
    }
}

/// A collection of tasks with a bounded size.
///
/// This is just a very thin wrapper around a `Vec` that ensures that the
/// nominal capacity bound is never exceeded.
#[derive(Debug)]
pub(crate) struct Bucket<T, const CAPACITY: usize>(Vec<T>);

impl<T, const CAPACITY: usize> Bucket<T, CAPACITY> {
    /// Creates a new bucket, allocating the full capacity upfront.
    pub(crate) fn new() -> Self {
        Self(Vec::with_capacity(CAPACITY))
    }

    /// Returns the bucket's nominal capacity.
    pub(crate) const fn capacity() -> usize {
        CAPACITY
    }

    /// Appends one task if capacity allows; otherwise returns the task in the
    /// error.
    pub(crate) fn push(&mut self, task: T) -> Result<(), T> {
        if self.0.len() < CAPACITY {
            self.0.push(task);
            Ok(())
        } else {
            Err(task)
        }
    }
}

impl<T, const CAPACITY: usize> IntoIterator for Bucket<T, CAPACITY> {
    type Item = T;
    type IntoIter = vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T, const CAPACITY: usize> FromIterator<T> for Bucket<T, CAPACITY> {
    fn from_iter<U: IntoIterator<Item = T>>(iter: U) -> Self {
        Self(Vec::from_iter(iter.into_iter().take(CAPACITY)))
    }
}
