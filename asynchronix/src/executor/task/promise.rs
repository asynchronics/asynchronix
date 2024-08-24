extern crate alloc;

use std::alloc::{dealloc, Layout};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::panic::{RefUnwindSafe, UnwindSafe};

use crate::loom_exports::sync::atomic::{self, Ordering};

use super::runnable::Runnable;
use super::util::{runnable_exists, RunOnDrop};
use super::Task;
use super::{CLOSED, POLLING, REF_INC, REF_MASK};

/// Virtual table for a `Promise`.
#[derive(Debug)]
struct VTable<U: Send + 'static> {
    poll: unsafe fn(*const ()) -> Stage<U>,
    drop: unsafe fn(*const ()),
}

/// Retrieves the output of the task if ready.
unsafe fn poll<F, S, T>(ptr: *const ()) -> Stage<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    let this = &*(ptr as *const Task<F, S, T>);

    // Set the `CLOSED` flag if the task is in the `Completed` phase.
    //
    // Ordering: Acquire ordering is necessary to synchronize with the
    // operation that modified or dropped the future or output. This ensures
    // that the newest state of the output is visible before it is moved
    // out, or that the future can be safely dropped when the promised is
    // dropped if the promise is the last reference to the task.
    let state = this
        .state
        .fetch_update(Ordering::Acquire, Ordering::Relaxed, |s| {
            if s & (POLLING | CLOSED) == 0 {
                Some(s | CLOSED)
            } else {
                None
            }
        });

    if let Err(s) = state {
        if s & CLOSED == CLOSED {
            // The task is either in the `Wind-down` or `Closed` phase.
            return Stage::Cancelled;
        } else {
            // The task is in the `Polling` phase.
            return Stage::Pending;
        };
    }

    let output = this.core.with_mut(|c| ManuallyDrop::take(&mut (*c).output));

    Stage::Ready(output)
}

/// Drops the promise.
unsafe fn drop<F, S, T>(ptr: *const ())
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    let this = &*(ptr as *const Task<F, S, T>);

    // Decrement the reference count.
    //
    // Ordering: Release ordering is necessary to ensure that if the output
    // was moved out by using `poll`, then the move has completed when the
    // last reference deallocates the task.
    let state = this.state.fetch_sub(REF_INC, Ordering::Release);

    // Deallocate the task if this token was the last reference to the task.
    if state & REF_MASK == REF_INC && !runnable_exists(state) {
        // Ensure that the newest state of the future or output is visible
        // before it is dropped.
        //
        // Ordering: Acquire ordering is necessary to synchronize with the
        // Release ordering in all previous reference count decrements
        // and/or in the wake count reset (the latter is equivalent to a
        // reference count decrement for a `Runnable`).
        atomic::fence(Ordering::Acquire);

        // Set a drop guard to ensure that the task is deallocated whether
        // or not the `core` member panics when dropped.
        let _drop_guard = RunOnDrop::new(|| {
            dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
        });

        if state & POLLING == POLLING {
            this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).future));
        } else if state & CLOSED == 0 {
            this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).output));
        }
        // Else the `CLOSED` flag is set but the `POLLING` flag is cleared
        // so the future was already dropped.
    }
}

/// The stage of progress of a promise.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) enum Stage<T> {
    /// The task has completed.
    Ready(T),
    /// The task is still being processed.
    Pending,
    /// The task has been cancelled.
    Cancelled,
}

impl<U> Stage<U> {
    /// Maps a `Stage<U>` to `Stage<V>` by applying a function to a contained value.
    #[allow(unused)]
    pub(crate) fn map<V, F>(self, f: F) -> Stage<V>
    where
        F: FnOnce(U) -> V,
    {
        match self {
            Stage::Ready(t) => Stage::Ready(f(t)),
            Stage::Pending => Stage::Pending,
            Stage::Cancelled => Stage::Cancelled,
        }
    }

    /// Returns `true` if the promise is a [`Stage::Ready`] value.
    #[allow(unused)]
    #[inline]
    pub(crate) fn is_ready(&self) -> bool {
        matches!(*self, Stage::Ready(_))
    }

    /// Returns `true` if the promise is a [`Stage::Pending`] value.
    #[allow(unused)]
    #[inline]
    pub(crate) fn is_pending(&self) -> bool {
        matches!(*self, Stage::Pending)
    }

    /// Returns `true` if the promise is a [`Stage::Cancelled`] value.
    #[allow(unused)]
    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(*self, Stage::Cancelled)
    }
}

/// A promise that can poll a task's output of type `U`.
///
/// Note that dropping a promise does not cancel the task.
#[derive(Debug)]
pub(crate) struct Promise<U: Send + 'static> {
    task: *const (),
    vtable: &'static VTable<U>,
}

impl<U: Send + 'static> Promise<U> {
    /// Creates a `Promise`.
    ///
    /// Safety: this is safe provided that:
    ///
    /// - the task pointer points to a live task allocated with the global
    ///   allocator,
    /// - the reference count has been incremented to account for this new task
    ///   reference.
    pub(super) unsafe fn new_unchecked<F, S, T>(task: *const Task<F, S, T>) -> Self
    where
        F: Future<Output = U> + Send + 'static,
        S: Fn(Runnable, T) + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
    {
        Self {
            task: task as *const (),
            vtable: &VTable::<U> {
                poll: poll::<F, S, T>,
                drop: drop::<F, S, T>,
            },
        }
    }

    /// Retrieves the output of the task if ready.
    #[allow(unused)]
    pub(crate) fn poll(&self) -> Stage<U> {
        unsafe { (self.vtable.poll)(self.task) }
    }
}

impl<U: Send + 'static> Drop for Promise<U> {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self.task) }
    }
}

unsafe impl<U: Send + 'static> Send for Promise<U> {}
impl<U: Send + 'static> UnwindSafe for Promise<U> {}
impl<U: Send + 'static> RefUnwindSafe for Promise<U> {}
