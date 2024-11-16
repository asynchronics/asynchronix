//! A primitive similar to a one-shot channel but without any signaling
//! capability.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::atomic::Ordering;

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::sync::atomic::{self, AtomicUsize};

// [C] Indicates whether the writer or the reader has been dropped.
const CLOSED: usize = 0b01;
// [P] Indicates whether a value is available (implies CLOSED).
const POPULATED: usize = 0b10;

// Possible states:
//
// | P | C |
// |---|---|
// | 0 | 0 |
// | 0 | 1 |
// | 1 | 1 |

/// The shared data of `SlotWriter` and `SlotReader`.
struct Inner<T> {
    /// A bit field for `CLOSED` and `POPULATED`.
    state: AtomicUsize,
    /// The value, if any.
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Inner<T> {
    // Sets the value without dropping the previous content.
    //
    // # Safety
    //
    // The caller must have exclusive access to the value.
    unsafe fn write_value(&self, t: T) {
        self.value.with_mut(|value| (*value).write(t));
    }

    // Reads the value without moving it.
    //
    // # Safety
    //
    // The value must be initialized and the caller must have exclusive access
    // to the value. After the call, the value slot within `Inner` should be
    // considered uninitialized in order to avoid a double-drop.
    unsafe fn read_value(&self) -> T {
        self.value.with(|value| (*value).as_ptr().read())
    }

    // Drops the value in place without deallocation.
    //
    // # Safety
    //
    // The value must be initialized and the caller must have exclusive access
    // to the value.
    unsafe fn drop_value_in_place(&self) {
        self.value
            .with_mut(|value| ptr::drop_in_place((*value).as_mut_ptr()));
    }
}

/// A handle to a slot that can write the value.
#[derive(Debug)]
pub(crate) struct SlotWriter<T> {
    /// The shared data.
    inner: NonNull<Inner<T>>,
    /// Drop checker hint: we may drop an `Inner<T>` and thus a `T`.
    _phantom: PhantomData<Inner<T>>,
}

impl<T> SlotWriter<T> {
    /// Writes a value to the slot.
    pub(crate) fn write(self, value: T) -> Result<(), WriteError> {
        // Prevent the drop handler from running.
        let this = ManuallyDrop::new(self);

        // Safety: it is safe to access `inner` as we did not set the `CLOSED`
        // flag.
        unsafe {
            this.inner.as_ref().write_value(value);

            // Ordering: this Release operation synchronizes with the Acquire
            // operations in `SlotReader::try_read` and in `SlotReader`'s drop
            // handler, ensuring that the value written is fully visible when it
            // is read.
            let state = this
                .inner
                .as_ref()
                .state
                .fetch_or(POPULATED | CLOSED, Ordering::Release);

            if state & CLOSED == CLOSED {
                // Ensure that all atomic accesses to the state have completed
                // before deallocation.
                //
                // Ordering: this Acquire fence synchronizes with the Release
                // operation in the drop handler of the `SlotReader` that set
                // the `CLOSED` flag.
                atomic::fence(Ordering::Acquire);

                // Drop the value written above.
                //
                // Safety: the value was just written and we have exclusive
                // access to it since the reader was dropped.
                this.inner.as_ref().drop_value_in_place();

                // Deallocate inner.
                drop(Box::from_raw(this.inner.as_ptr()));

                Err(WriteError {})
            } else {
                Ok(())
            }
        }
    }
}

impl<T> Drop for SlotWriter<T> {
    fn drop(&mut self) {
        // Safety: it is safe to access `inner` as we did not set the `CLOSED`
        // flag.
        unsafe {
            // Ordering: Acquire ordering is necessary in case the `CLOSED` flag
            // is set: it synchronizes with the Release operation in the drop
            // handler of the `SlotReader` that set the `CLOSED` flag and
            // ensures that the all accesses to the slot have completed before
            // deallocation.
            let mut state = self.inner.as_ref().state.load(Ordering::Acquire);

            // Close the slot if it isn't already.
            //
            // Ordering: Acquire ordering in case the `CLOSED` flag was set just
            // after the state was loaded above, for the reasons stated as
            // above. Release ordering is in turn necessary in the expected case
            // where the `CLOSED` flag is now set: it synchronizes with the
            // Acquire operation in the drop handler of the `SlotReader` and
            // ensures that this access to the slot has completed before the
            // `SlotReader` performs deallocation.
            if state & CLOSED == 0 {
                state = self.inner.as_ref().state.fetch_or(CLOSED, Ordering::AcqRel);

                // The reader is alive, so let it handle the cleanup.
                if state & CLOSED == 0 {
                    return;
                }
            }

            // Deallocate the slot since it was closed by the reader.
            //
            // Note: there can't be any value because `write` consumes the writer
            // and does not run the drop handler.
            //
            // Safety: `inner` will no longer be used once deallocated.
            drop(Box::from_raw(self.inner.as_ptr()));
        }
    }
}

unsafe impl<T: Send> Send for SlotWriter<T> {}
unsafe impl<T: Send> Sync for SlotWriter<T> {}

impl<T> UnwindSafe for SlotWriter<T> {}
impl<T> RefUnwindSafe for SlotWriter<T> {}

/// A handle to a slot that can read the value.
#[derive(Debug)]
pub(crate) struct SlotReader<T> {
    /// The shared data.
    inner: NonNull<Inner<T>>,
    /// Drop checker hint: we may drop an `Inner<T>` and thus a `T`.
    _phantom: PhantomData<Inner<T>>,
}

impl<T> SlotReader<T> {
    /// Attempts to read the value.
    pub(crate) fn try_read(&mut self) -> Result<T, ReadError> {
        // Safety: it is safe to access `inner` as we did not set the `CLOSED`
        // flag.
        unsafe {
            // Ordering: this Acquire load synchronizes with the Release
            // operation in `SlotWriter::write`, ensuring that the value written
            // is fully visible when the `POPULATED` flag is read.
            let state = self.inner.as_ref().state.load(Ordering::Acquire);

            // If there is no value but the writer is still alive, return `NoValue`.
            if state == 0 {
                return Err(ReadError::NoValue);
            }

            // If there is no value and the writer was dropped, return `Closed`.
            if state & POPULATED == 0 {
                return Err(ReadError::Closed);
            }

            // At this point, we know that `POPULATED`, and therefore `CLOSED`, are
            // set.

            // Clear the `POPULATED` flag since we are going to take the value.
            //
            // Ordering: there is no need for further synchronization since the
            // above Acquire load already ensures that the value is visible and
            // the value will no longer be used. The value of the `POPULATED`
            // flag is only observed by this thread.
            self.inner.as_ref().state.store(CLOSED, Ordering::Relaxed);

            // Safety: we know there is a value and that it is fully visible.
            Ok(self.inner.as_ref().read_value())
        }
    }
}

impl<T> Drop for SlotReader<T> {
    fn drop(&mut self) {
        // Safety: it is safe to access `inner` as we did not set the `CLOSED`
        // flag.
        unsafe {
            // Ordering: Acquire ordering is necessary in case the `CLOSED` flag
            // is set: it synchronizes with the Release operation in the drop
            // handler of the `SlotWriter` that set the `CLOSED` flag and
            // ensures that the all accesses to the slot have completed before
            // the value is dropped and the slot is deallocated.
            let mut state = self.inner.as_ref().state.load(Ordering::Acquire);

            // Close the slot if it isn't already.
            if state & CLOSED == 0 {
                // Ordering: this Acquire operation synchronizes with the
                // Release operation in `SlotWriter::write`, ensuring that the
                // value written is fully visible in case it needs to be
                // dropped. Release ordering is in turn necessary in the
                // expected case where the `CLOSED` flag is now set: it
                // synchronizes with the Acquire operation in the `write` method
                // or the drop handler of the `SlotWriter` and ensures that this
                // access to the slot has completed before the `SlotWriter`
                // performs deallocation.
                state = self.inner.as_ref().state.fetch_or(CLOSED, Ordering::AcqRel);

                // The writer is alive, so let it handle the cleanup.
                if state & CLOSED == 0 {
                    return;
                }
            }

            // Drop the value if necessary and deallocate the slot since it was
            // closed by the writer.
            //
            // Safety: `inner` will no longer be used once deallocated. If there
            // is an unread value, drop it first.
            if state & POPULATED == POPULATED {
                // Safety: the presence of an initialized value was just checked
                // and there is no live writer so no risk of race.
                self.inner.as_ref().drop_value_in_place();
            }
            drop(Box::from_raw(self.inner.as_ptr()));
        }
    }
}

unsafe impl<T: Send> Send for SlotReader<T> {}
unsafe impl<T: Send> Sync for SlotReader<T> {}

impl<T> UnwindSafe for SlotReader<T> {}
impl<T> RefUnwindSafe for SlotReader<T> {}

/// Error returned when reading a value fails.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ReadError {
    /// The slot does not contain any value yet.
    NoValue,
    /// The writer was dropped or the value was already taken.
    Closed,
}

impl fmt::Display for ReadError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoValue => write!(fmt, "no value in the slot"),
            Self::Closed => write!(fmt, "slot closed by writer"),
        }
    }
}

impl Error for ReadError {}

/// Error returned when writing a value fails due to the reader being dropped.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct WriteError {}

impl fmt::Display for WriteError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "slot closed by reader")
    }
}

impl Error for WriteError {}

/// Creates the writer and reader handles of a slot.
pub(crate) fn slot<T>() -> (SlotWriter<T>, SlotReader<T>) {
    let inner = NonNull::new(Box::into_raw(Box::new(Inner {
        state: AtomicUsize::new(0),
        value: UnsafeCell::new(MaybeUninit::uninit()),
    })))
    .unwrap();

    let writer = SlotWriter {
        inner,
        _phantom: PhantomData,
    };

    let reader = SlotReader {
        inner,
        _phantom: PhantomData,
    };

    (writer, reader)
}

#[cfg(all(test, not(nexosim_loom)))]
mod tests {
    use super::*;

    use std::thread;

    #[test]
    fn slot_single_threaded_write() {
        let (writer, mut reader) = slot();

        assert_eq!(reader.try_read(), Err(ReadError::NoValue));
        assert!(writer.write(42).is_ok());
        assert_eq!(reader.try_read(), Ok(42));
    }

    #[test]
    fn slot_single_threaded_drop_writer() {
        let (writer, mut reader) = slot::<i32>();

        assert_eq!(reader.try_read(), Err(ReadError::NoValue));
        drop(writer);
        assert_eq!(reader.try_read(), Err(ReadError::Closed));
    }

    #[test]
    fn slot_single_threaded_drop_reader() {
        let writer = slot().0;

        assert!(writer.write(42).is_err());
    }

    #[test]
    fn slot_multi_threaded_write() {
        let (writer, mut reader) = slot();

        thread::spawn(move || {
            assert!(writer.write(42).is_ok());
        });

        loop {
            if let Ok(v) = reader.try_read() {
                assert_eq!(v, 42);
                return;
            }
        }
    }

    #[test]
    fn slot_multi_threaded_drop_writer() {
        let (writer, mut reader) = slot::<i32>();

        thread::spawn(move || {
            drop(writer);
        });

        loop {
            let v = reader.try_read();
            assert!(v.is_err());
            if v == Err(ReadError::Closed) {
                return;
            }
        }
    }
}

#[cfg(all(test, nexosim_loom))]
mod tests {
    use super::*;

    use loom::model::Builder;
    use loom::thread;

    #[test]
    fn loom_slot_write() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (writer, mut reader) = slot();

            let th = thread::spawn(move || assert!(writer.write(42).is_ok()));

            if let Ok(v) = reader.try_read() {
                assert_eq!(v, 42);
            }

            th.join().unwrap();
        });
    }

    #[test]
    fn loom_slot_drop_writer() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (writer, mut reader) = slot::<i32>();

            let th = thread::spawn(move || drop(writer));

            assert!(reader.try_read().is_err());

            th.join().unwrap();
        });
    }
}
