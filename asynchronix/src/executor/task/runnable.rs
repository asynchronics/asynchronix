extern crate alloc;

use std::alloc::{dealloc, Layout};
use std::future::Future;
use std::mem::{self, ManuallyDrop};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, Waker};

use crate::loom_exports::debug_or_loom_assert;
use crate::loom_exports::sync::atomic::{self, AtomicU64, Ordering};

use super::util::RunOnDrop;
use super::{raw_waker_vtable, Task};
use super::{CLOSED, POLLING, REF_MASK, WAKE_MASK};

/// Virtual table for a `Runnable`.
#[derive(Debug)]
struct VTable {
    run: unsafe fn(*const ()),
    cancel: unsafe fn(*const ()),
}

/// Polls the inner future.
unsafe fn run<F, S, T>(ptr: *const ())
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    let this = &*(ptr as *const Task<F, S, T>);

    // A this point, the task cannot be in the `Completed` phase, otherwise
    // it would not have been scheduled in the first place. It could,
    // however, have been cancelled and transitioned from `Polling` to
    // `Wind-down` after it was already scheduled. It is possible that in
    // such case the `CLOSED` flag may not be visible when loading the
    // state, but this is not a problem: when a task is cancelled while
    // already scheduled (i.e. while the wake count is non-zero), its future
    // is kept alive so even if the state loaded is stale, the worse that
    // can happen is that the future will be unnecessarily polled.
    //
    // It is worth mentioning that, in order to detect if the task was
    // awaken while polled, other executors reset a notification flag with
    // an RMW when entering `run`. The idea here is to avoid such RMW and
    // instead load a wake count. Only once the task has been polled, an RMW
    // checks the wake count again to detect if the task was notified in the
    // meantime. This method may be slightly more prone to spurious false
    // positives but is much faster (1 vs 2 RMWs) and still prevent the
    // occurrence of lost wake-ups.

    // Load the state.
    //
    // Ordering: the below Acquire load synchronizes with the Release
    // operation at the end of the call to `run` by the previous `Runnable`
    // and ensures that the new state of the future stored by the previous
    // call to `run` is visible. This synchronization exists because the RMW
    // in the call to `Task::wake` or `Task::wake_by_ref` that scheduled
    // this `Runnable` establishes a Release sequence. This load also
    // synchronizes with the Release operation in `wake` and ensures that
    // all memory operations performed by their callers are visible. Since
    // this is a simple load, it may be stale and some wake requests may not
    // be visible yet, but the post-polling RMW will later check if all wake
    // requests were serviced.
    let mut state = this.state.load(Ordering::Acquire);
    let mut wake_count = state & WAKE_MASK;

    debug_or_loom_assert!(state & POLLING == POLLING);

    loop {
        // Drop the future if the phase has transitioned to `Wind-down`.
        if state & CLOSED == CLOSED {
            cancel::<F, S, T>(ptr);

            return;
        }

        // Poll the task.
        let raw_waker = RawWaker::new(ptr, raw_waker_vtable::<F, S, T>());
        let waker = ManuallyDrop::new(Waker::from_raw(raw_waker));

        let cx = &mut Context::from_waker(&waker);
        let fut = Pin::new_unchecked(this.core.with_mut(|c| &mut *(*c).future));

        // Set a panic guard to cancel the task if the future panics when
        // polled.
        let panic_guard = RunOnDrop::new(|| cancel::<F, S, T>(ptr));

        let poll_state = fut.poll(cx);
        mem::forget(panic_guard);

        if let Poll::Ready(output) = poll_state {
            // Set a panic guard to close the task if the future or the output
            // panic when dropped. Miri complains if a reference to `this` is
            // captured and `mem::forget` is called on the guard after
            // deallocation, which is why the state is taken by pointer.
            let state_ptr = &this.state as *const AtomicU64;
            let panic_guard = RunOnDrop::new(|| {
                // Clear the `POLLING` flag while setting the `CLOSED` flag
                // to enter the `Closed` phase.
                //
                // Ordering: Release ordering on success is necessary to
                // ensure that all memory operations on the future or the
                // output are visible when the last reference deallocates
                // the task.
                let state = (*state_ptr)
                    .fetch_update(Ordering::Release, Ordering::Relaxed, |s| {
                        Some((s | CLOSED) & !POLLING)
                    })
                    .unwrap();

                // Deallocate if there are no more references to the task.
                if state & REF_MASK == 0 {
                    // Ensure that all atomic accesses to the state are
                    // visible.
                    //
                    // Ordering: this Acquire fence synchronizes with all
                    // Release operations that decrement the number of
                    // references to the task.
                    atomic::fence(Ordering::Acquire);

                    dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
                }
            });

            // Drop the future and publish its output.
            this.core.with_mut(|c| {
                ManuallyDrop::drop(&mut (*c).future);
                (*c).output = ManuallyDrop::new(output);
            });

            // Clear the `POLLING` flag to enter the `Completed` phase,
            // unless the task has concurrently transitioned to the
            // `Wind-down` phase or unless this `Runnable` is the last
            // reference to the task.
            if this
                .state
                .fetch_update(Ordering::Release, Ordering::Relaxed, |s| {
                    if s & CLOSED == CLOSED || s & REF_MASK == 0 {
                        None
                    } else {
                        Some(s & !POLLING)
                    }
                })
                .is_ok()
            {
                mem::forget(panic_guard);
                return;
            }

            // The task is in the `Wind-down` phase or this `Runnable`
            // was the last reference, so the output must be dropped.
            this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).output));
            mem::forget(panic_guard);

            // Clear the `POLLING` flag to enter the `Closed` phase. This is
            // not actually necessary if the `Runnable` is the last
            // reference, but that should be a very rare occurrence.
            //
            // Ordering: Release ordering is necessary to ensure that the
            // drop of the output is visible when the last reference
            // deallocates the task.
            state = this.state.fetch_and(!POLLING, Ordering::Release);

            // Deallocate the task if there are no task references left.
            if state & REF_MASK == 0 {
                // Ensure that all atomic accesses to the state are visible.
                //
                // Ordering: this Acquire fence synchronizes with all
                // Release operations that decrement the number of
                // references to the task.
                atomic::fence(Ordering::Acquire);
                dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
            }

            return;
        }

        // The future is `Pending`: try to reset the wake count.
        //
        // Ordering: a Release ordering is required in case the wake count
        // is successfully cleared; it synchronizes, via a Release sequence,
        // with the Acquire load upon entering `Runnable::run` the next time
        // it is called. Acquire ordering is in turn necessary in case the
        // wake count has changed and the future must be polled again; it
        // synchronizes with the Release RMW in `wake` and ensures that all
        // memory operations performed by their callers are visible when the
        // polling loop is repeated.
        state = this.state.fetch_sub(wake_count, Ordering::AcqRel);
        debug_or_loom_assert!(state > wake_count);
        wake_count = (state & WAKE_MASK) - wake_count;

        // Return now if the wake count has been successfully cleared,
        // provided that the task was not concurrently cancelled.
        if wake_count == 0 && state & CLOSED == 0 {
            // If there are no task references left, cancel and deallocate
            // the task since it can never be scheduled again.
            if state & REF_MASK == 0 {
                let _drop_guard = RunOnDrop::new(|| {
                    dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
                });

                // Drop the future;
                this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).future));
            }

            return;
        }
    }
}

/// Cancels the task, dropping the inner future.
unsafe fn cancel<F, S, T>(ptr: *const ())
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    let this = &*(ptr as *const Task<F, S, T>);

    // Ensure that the modifications of the future by the previous
    // `Runnable` are visible.
    //
    // Ordering: this Acquire fence synchronizes with the Release operation
    // at the end of the call to `run` by the previous `Runnable` and
    // ensures that the new state of the future stored by the previous call
    // to `run` is visible. This synchronization exists because the wake
    // count RMW in the call to `Task::wake` that created this `Runnable`
    // establishes a Release sequence.
    atomic::fence(Ordering::Acquire);

    // Set a drop guard to enter the `Closed` phase whether or not the
    // future panics when dropped.
    let _drop_guard = RunOnDrop::new(|| {
        // Clear the `POLLING` flag while setting the `CLOSED` flag to enter
        // the `Closed` phase.
        //
        // Ordering: Release ordering on success is necessary to ensure that
        // all memory operations on the future are visible when the last
        // reference deallocates the task.
        let state = this
            .state
            .fetch_update(Ordering::Release, Ordering::Relaxed, |s| {
                Some((s | CLOSED) & !POLLING)
            })
            .unwrap();

        // Deallocate if there are no more references to the task.
        if state & REF_MASK == 0 {
            // Ensure that all atomic accesses to the state are visible.
            //
            // Ordering: this Acquire fence synchronizes with all Release
            // operations that decrement the number of references to the
            // task.
            atomic::fence(Ordering::Acquire);
            dealloc(ptr as *mut u8, Layout::new::<Task<F, S, T>>());
        }
    });

    // Drop the future;
    this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).future));
}

/// Handle to a scheduled task.
///
/// Dropping the runnable directly instead of calling `run` cancels the task.
#[derive(Debug)]
pub(crate) struct Runnable {
    task: *const (),
    vtable: &'static VTable,
}

impl Runnable {
    /// Creates a `Runnable`.
    ///
    /// Safety: this is safe provided that:
    ///
    /// - the task pointer points to a live task allocated with the global
    ///   allocator,
    /// - there is not other live `Runnable` for this task,
    /// - the wake count is non-zero,
    /// - the `POLLING` flag is set and the `CLOSED` flag is cleared,
    /// - the task contains a live future.
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
                run: run::<F, S, T>,
                cancel: cancel::<F, S, T>,
            },
        }
    }

    /// Polls the wrapped future.
    pub(crate) fn run(self) {
        // Prevent the drop handler from being called, as it would call `cancel`
        // on the inner field.
        let this = ManuallyDrop::new(self);

        // Poll the future.
        unsafe { (this.vtable.run)(this.task) }
    }
}

impl Drop for Runnable {
    fn drop(&mut self) {
        // Cancel the task.
        unsafe { (self.vtable.cancel)(self.task) }
    }
}

unsafe impl Send for Runnable {}
impl UnwindSafe for Runnable {}
impl RefUnwindSafe for Runnable {}
