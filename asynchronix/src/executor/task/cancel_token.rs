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

/// Virtual table for a `CancelToken`.
#[derive(Debug)]
struct VTable {
    cancel: unsafe fn(*const ()),
    drop: unsafe fn(*const ()),
}

/// Cancels a pending task.
///
/// If the task is completed, nothing is done. If the task is not completed
/// but not currently scheduled (no `Runnable` exist) then the future is
/// dropped immediately. Otherwise, the future will be dropped at a later
/// time by the scheduled `Runnable` once it runs.
unsafe fn cancel<F, S, T>(ptr: *const ())
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    let this = &*(ptr as *const Task<F, S, T>);

    // Enter the `Closed` or `Wind-down` phase if the tasks is not
    // completed.
    //
    // Ordering: Acquire ordering is necessary to synchronize with any
    // operation that modified or dropped the future or output. This ensures
    // that the future or output can be safely dropped or that the task can
    // be safely deallocated if necessary. The Release ordering synchronizes
    // with any of the Acquire atomic fences and ensure that this atomic
    // access is fully completed upon deallocation.
    let state = this
        .state
        .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |s| {
            if s & POLLING == 0 {
                // The task has completed or is closed so there is no need
                // to drop the future or output and the reference count can
                // be decremented right away.
                Some(s - REF_INC)
            } else if runnable_exists(s) {
                // A `Runnable` exists so the future cannot be dropped (this
                // will be done by the `Runnable`) and the reference count
                // can be decremented right away.
                Some((s | CLOSED) - REF_INC)
            } else {
                // The future or the output needs to be dropped so the
                // reference count cannot be decremented just yet, otherwise
                // another reference could deallocate the task before the
                // drop is complete.
                Some((s | CLOSED) & !POLLING)
            }
        })
        .unwrap();

    if runnable_exists(state) {
        // The task is in the `Wind-down` phase so the cancellation is now
        // the responsibility of the current `Runnable`.
        return;
    }

    if state & POLLING == 0 {
        // Deallocate the task if this was the last reference.
        if state & REF_MASK == REF_INC {
            // Ensure that all atomic accesses to the state are visible.

            // FIXME: the fence does not seem necessary since the fetch_update
            // uses AcqRel.
            //
            // Ordering: this Acquire fence synchronizes with all Release
            // operations that decrement the number of references to the task.
            atomic::fence(Ordering::Acquire);

            // Set a drop guard to ensure that the task is deallocated,
            // whether or not the output panics when dropped.
            let _drop_guard = RunOnDrop::new(|| {
                dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
            });

            // Drop the output if any.
            if state & CLOSED == 0 {
                this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).output));
            }
        }

        return;
    }

    // Set a drop guard to ensure that reference count is decremented and
    // the task is deallocated if this is the last reference, whether or not
    // the future panics when dropped.
    let _drop_guard = RunOnDrop::new(|| {
        // Ordering: Release ordering is necessary to ensure that the drop
        // of the future or output is visible when the last reference
        // deallocates the task.
        let state = this.state.fetch_sub(REF_INC, Ordering::Release);
        if state & REF_MASK == REF_INC {
            // Ensure that all atomic accesses to the state are visible.
            //
            // Ordering: this Acquire fence synchronizes with all Release
            // operations that decrement the number of references to the
            // task.
            atomic::fence(Ordering::Acquire);

            dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
        }
    });

    this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).future));
}

/// Drops the token without cancelling the task.
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
    // Ordering: the Release ordering synchronizes with any of the Acquire
    // atomic fences and ensure that this atomic access is fully completed
    // upon deallocation.
    let state = this.state.fetch_sub(REF_INC, Ordering::Release);

    // Deallocate the task if this token was the last reference to the task.
    if state & REF_MASK == REF_INC && !runnable_exists(state) {
        // Ensure that the newest state of the future or output is visible
        // before it is dropped.
        //
        // Ordering: this Acquire fence synchronizes with all Release
        // operations that decrement the number of references to the task.
        atomic::fence(Ordering::Acquire);

        // Set a drop guard to ensure that the task is deallocated whether
        // or not the future or output panics when dropped.
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

/// A token that can be used to cancel a task.
#[derive(Debug)]
pub(crate) struct CancelToken {
    task: *const (),
    vtable: &'static VTable,
}

impl CancelToken {
    /// Creates a `CancelToken`.
    ///
    /// Safety: this is safe provided that:
    ///
    /// - the task pointer points to a live task allocated with the global
    ///   allocator,
    /// - the reference count has been incremented to account for this new task
    ///   reference.
    pub(super) unsafe fn new_unchecked<F, S, T>(task: *const Task<F, S, T>) -> Self
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
        S: Fn(Runnable, T) + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
    {
        Self {
            task: task as *const (),
            vtable: &VTable {
                cancel: cancel::<F, S, T>,
                drop: drop::<F, S, T>,
            },
        }
    }

    /// Cancels the task.
    ///
    /// If the task is completed, nothing is done. If the task is not completed
    /// but not currently scheduled (no `Runnable` exist) then the future is
    /// dropped immediately. Otherwise, the future will be dropped at a later
    /// time by the scheduled `Runnable` once it runs.
    pub(crate) fn cancel(self) {
        // Prevent the drop handler from being called, as it would call
        // `drop_token` on the inner field.
        let this = ManuallyDrop::new(self);

        unsafe { (this.vtable.cancel)(this.task) }
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self.task) }
    }
}

unsafe impl Send for CancelToken {}
impl UnwindSafe for CancelToken {}
impl RefUnwindSafe for CancelToken {}
