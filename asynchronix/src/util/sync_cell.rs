//! Very efficient, single-writer alternative to `RwLock` based on a fully safe
//! seqlock implementation.

#![allow(unused)]

use std::cell::Cell;
use std::marker::PhantomData;

use crate::loom_exports::sync::atomic::{self, AtomicUsize, Ordering};
use crate::loom_exports::sync::Arc;

/// An adapter to a `Value` type which may safely read and write a value from
/// multiple threads without synchronization, but which only guarantees that the
/// value from a read is valid when all write and read accesses were
/// synchronized.
///
/// Taking for instance a value composed of several `u64` fields, then its
/// `TearableAtomic` adaptor could using simple relaxed loads and stores on
/// several `AtomicU64`. While `tearable_store`/`tearable_read` could possibly
/// store/read inconsistent values when racing with a writer, they should always
/// store/read consistent values in the absence of a race.
///
/// This trait is meant to enable optimistic reads of the value and discard such
/// reads whenever a race could have taken place.
pub(crate) trait TearableAtomic: Sync {
    /// The value to be read and written.
    type Value;

    /// Reads a value which is guaranteed to be the same as the last written
    /// value, provided there were no races when writing and reading.
    ///
    /// If an inconsistent value is produced as the result of a torn load,
    /// however, its construction and destruction should neither lead to UB nor
    /// produce a panic or other unwanted side-effects.
    fn tearable_load(&self) -> Self::Value;

    /// Writes a value which is guaranteed to remain unchanged until it is read
    /// back, provided there were no races when writing and reading.
    ///
    /// If an inconsistent value is produced as the result of a torn store,
    /// however, its construction and destruction should neither lead to UB nor
    /// produce a panic or other unwanted side-effects.
    fn tearable_store(&self, value: Self::Value);
}

/// The inner type of `SyncCell` and `SyncCellReader`.
struct Inner<T: TearableAtomic> {
    tearable: T,
    sequence: AtomicUsize,
}

/// A single-writer, multiple-readers synchronized cell based on a fully safe
/// seqlock implementation.
///
/// Yes, there are already crates that implement seqlocks for arbitrary types,
/// but as of today these either need to rely on UB or have various
/// shortcomings. See in particular this RFC, which intends to eventually bring
/// a proper solution to this problem:
///
/// <https://github.com/rust-lang/rfcs/pull/3301>.
///
/// In the meantime, this implementation sidesteps these issues by only dealing
/// with values that implement the `TearableAtomic` trait, which basically means
/// values which may become inconsistent due to torn stores or loads but which
/// can still be constructed and destructed even in such case.
///
/// Note that it is still possible to use a `SyncCell` for types that cannot be
/// safely constructed from a teared state: it is enough to make
/// `TearableAtomic::Value` an always-safe-to-construct builder type for the
/// actual value, and to build the actual value only when a builder is returned
/// from the `read` method since such builder is then guaranteed to be in a
/// valid state.
///
/// `SyncCell` is restricted to a single writer, which is the `SyncCell` object
/// itself. This makes it possible to increment the sequence count with simple
/// loads and stores instead of more expensive read-modify-write atomic
/// operations. It also gives the `SyncCell` object the possibility to read the
/// value at any time without any synchronization overhead. Multiple thread-safe
/// reader handles can be constructed using the `reader` method.
pub(crate) struct SyncCell<T: TearableAtomic> {
    inner: Arc<Inner<T>>,
    _non_sync_phantom: PhantomData<Cell<()>>,
}

impl<T: TearableAtomic> SyncCell<T> {
    /// Creates a synchronized cell.
    pub(crate) fn new(tearable: T) -> Self {
        Self {
            inner: Arc::new(Inner {
                tearable,
                sequence: AtomicUsize::new(0),
            }),
            _non_sync_phantom: PhantomData,
        }
    }

    /// Performs a synchronized read.
    pub(crate) fn read(&self) -> T::Value {
        // The below read is always synchronized since `SyncCell` is `!Sync` and
        // therefore there cannot be concurrent write operations.
        self.inner.tearable.tearable_load()
    }

    /// Performs a synchronized write.
    pub(crate) fn write(&self, value: T::Value) {
        // Increment the sequence count to an odd number.
        //
        // Note: this thread is the only one that can change the sequence count
        // so even a plain load will always return the last sequence count.
        let seq = self.inner.sequence.load(Ordering::Relaxed);
        self.inner
            .sequence
            .store(seq.wrapping_add(1), Ordering::Relaxed);

        // Store the value.
        //
        // Ordering: this Release fence synchronizes with the `Acquire` fence in
        // `SyncCellReader::try_read` and ensures that either the above
        // increment to an odd sequence count is visible after the value is
        // tentatively read, or a later increment of the sequence count.
        atomic::fence(Ordering::Release);
        self.inner.tearable.tearable_store(value);

        // Increment the sequence count to an even number.
        //
        // Ordering: this Release store synchronizes with the Acquire load of
        // the sequence count at the beginning of `SyncCellReader::try_read` and
        // ensure that if the sequence count loaded is indeed even, then the
        // value has been fully written (though it may have been later
        // overwritten).
        self.inner
            .sequence
            .store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Returns a reader handle.
    pub(crate) fn reader(&self) -> SyncCellReader<T> {
        SyncCellReader {
            inner: self.inner.clone(),
        }
    }
}

/// A handle to a `SyncCell` that enables synchronized reads from multiple
/// threads.
#[derive(Clone)]
pub(crate) struct SyncCellReader<T: TearableAtomic> {
    inner: Arc<Inner<T>>,
}

impl<T: TearableAtomic> SyncCellReader<T> {
    /// Attempts a synchronized read.
    ///
    /// An error is returned if this read operation raced with a write
    /// operation.
    pub(crate) fn try_read(&self) -> Result<T::Value, SyncCellReadError> {
        // Read the initial sequence count and make sure it is even.
        //
        // Ordering: this Acquire load synchronizes with the Release store of an
        // even sequence count at the end of `SyncCell::write` and ensure that
        // if the sequence count is indeed even, then the value stored before
        // the sequence count was set was fully written (though it may have been
        // later overwritten).
        let seq = self.inner.sequence.load(Ordering::Acquire);
        if seq & 1 != 0 {
            return Err(SyncCellReadError {});
        }

        // Attempt to load the value, which may be torn if there is a concurrent
        // write operation.
        let value = self.inner.tearable.tearable_load();

        // Ordering: this Acquire fence synchronizes with the Release fence in
        // `SyncCell::write` and ensures that the below read of the sequence
        // count sees the increment to an odd sequence count the precedes the
        // tearable store, or a later increment of the sequence count.
        atomic::fence(Ordering::Acquire);

        // Check that the sequence count has not changed.
        let new_seq = self.inner.sequence.load(Ordering::Relaxed);
        if new_seq == seq {
            Ok(value)
        } else {
            Err(SyncCellReadError {})
        }
    }
}

/// An error returned when attempting to perform a read operation concurrently
/// with a write operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SyncCellReadError {}

/// Loom tests.
#[cfg(all(test, asynchronix_loom))]
mod tests {
    use super::*;

    use loom::lazy_static;
    use loom::model::Builder;
    use loom::sync::atomic::AtomicBool;
    use loom::thread;

    struct TestTearable<const N: usize> {
        inner: [AtomicUsize; N],
    }

    impl<const N: usize> TestTearable<N> {
        fn new(value: [usize; N]) -> Self {
            let inner: Vec<_> = value.into_iter().map(|v| AtomicUsize::new(v)).collect();

            Self {
                inner: inner.try_into().unwrap(),
            }
        }
    }

    impl<const N: usize> TearableAtomic for TestTearable<N> {
        type Value = [usize; N];

        fn tearable_load(&self) -> Self::Value {
            let mut value = [0usize; N];
            for i in 0..N {
                value[i] = self.inner[i].load(Ordering::Relaxed);
            }

            value
        }

        fn tearable_store(&self, value: Self::Value) {
            for i in 0..N {
                self.inner[i].store(value[i], Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn loom_sync_cell_race() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;

        const VALUE1: [usize; 3] = [1, 2, 3];
        const VALUE2: [usize; 3] = [4, 5, 6];
        const VALUE3: [usize; 3] = [7, 8, 9];

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let tearable = TestTearable::new(VALUE1);
            let cell = SyncCell::new(tearable);
            let reader = cell.reader();

            let th = thread::spawn(move || {
                if let Ok(v) = reader.try_read() {
                    assert!(v == VALUE1 || v == VALUE2 || v == VALUE3, "v = {:?}", v);
                }
            });

            cell.write(VALUE2);
            cell.write(VALUE3);
            th.join().unwrap();
        });
    }

    #[test]
    fn loom_sync_cell_synchronized() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;

        const VALUE1: [usize; 3] = [1, 2, 3];
        const VALUE2: [usize; 3] = [4, 5, 6];
        const VALUE3: [usize; 3] = [7, 8, 9];

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            lazy_static! {
                static ref NEW_VALUE_FLAG: AtomicBool = AtomicBool::new(false);
            };

            let tearable = TestTearable::new(VALUE1);
            let cell = SyncCell::new(tearable);
            let reader = cell.reader();

            let th = thread::spawn(move || {
                if NEW_VALUE_FLAG.load(Ordering::Acquire) {
                    let v = reader
                        .try_read()
                        .expect("read should always succeed when synchronized");
                    assert!(v == VALUE2 || v == VALUE3, "v = {:?}", v);

                    NEW_VALUE_FLAG.store(false, Ordering::Release);

                    if NEW_VALUE_FLAG.load(Ordering::Acquire) {
                        let v = reader
                            .try_read()
                            .expect("read should always succeed when synchronized");
                        assert_eq!(v, VALUE3);
                    }
                }
            });

            cell.write(VALUE2);
            NEW_VALUE_FLAG.store(true, Ordering::Release);

            if !NEW_VALUE_FLAG.load(Ordering::Acquire) {
                cell.write(VALUE3);
                NEW_VALUE_FLAG.store(true, Ordering::Release);
            }

            th.join().unwrap();
        });
    }
}
