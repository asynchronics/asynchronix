extern crate alloc;

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::future::Future;
use std::mem::{self, ManuallyDrop};
use std::task::{RawWaker, RawWakerVTable};

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::sync::atomic::{self, AtomicU64, Ordering};

mod cancel_token;
mod promise;
mod runnable;
mod util;

#[cfg(test)]
mod tests;

pub(crate) use cancel_token::CancelToken;
pub(crate) use promise::Promise;
pub(crate) use runnable::Runnable;

use self::util::{runnable_exists, RunOnDrop};

/// Flag indicating that the future has not been polled to completion yet.
const POLLING: u64 = 1 << 0;
/// Flag indicating that the task has been cancelled or that the output has
/// already been moved out.
const CLOSED: u64 = 1 << 1;
/// A single reference count increment.
const REF_INC: u64 = 1 << 2;
/// A single wake count increment.
const WAKE_INC: u64 = 1 << 33;
/// Reference count mask.
const REF_MASK: u64 = !(REF_INC - 1) & (WAKE_INC - 1);
/// Wake count mask.
const WAKE_MASK: u64 = !(WAKE_INC - 1);
/// Critical value of the reference count at which preventive measures must be
/// enacted to prevent counter overflow.
const REF_CRITICAL: u64 = (REF_MASK / 2) & REF_MASK;
/// Critical value of the wake count at which preventive measures must be
/// enacted to prevent counter overflow.
const WAKE_CRITICAL: u64 = (WAKE_MASK / 2) & WAKE_MASK;

/// Either a future, its output, or uninitialized (empty).
union TaskCore<F: Future> {
    /// Field present during the `Polling` and  the `Wind-down` phases.
    future: ManuallyDrop<F>,

    /// Field present during the `Completed` phase.
    output: ManuallyDrop<F::Output>,
}

/// A task.
///
/// A task contains both the scheduling function and the future to be polled (or
/// its output if available). `Waker`, `Runnable`, `Promise` and `CancelToken`
/// are all type-erased (fat) pointers to a `Task`. The task is automatically
/// deallocated when all the formers have been dropped.
///
/// The lifetime of a task involves up to 4 phases:
/// - `Polling` phase: the future needs to be polled,
/// - `Completed` phase: the future has been polled to completion and its output
///   is available,
/// - `Wind-down` phase: the task has been cancelled while it was already
///   scheduled for processing, so the future had to be kept temporarily alive
///   to avoid a race; the `Closed` phase will be entered only when the
///   scheduled task is processed,
/// - `Closed` phase: neither the future nor its output are available, either
///   because the task has been cancelled or because the output has been moved
///   out.
///
/// It is possible to move from `Polling` to `Completed`, `Wind-down` or
/// `Closed`, but the only possible transition from `Wind-down` and from
/// `Completed` is to `Closed`.
///
/// The different states and sub-states and their corresponding flags are
/// summarized below:
///
/// | Phase               | CLOSED | POLLING | WAKE_COUNT | Runnable exists? |
/// |---------------------|--------|---------|------------|------------------|
/// | Polling (idle)      |   0    |    1    |      0     |       No         |
/// | Polling (scheduled) |   0    |    1    |     ≠0     |       Yes        |
/// | Completed           |   0    |    0    |     any    |       No         |
/// | Wind-down           |   1    |    1    |     any    |       Yes        |
/// | Closed              |   1    |    0    |     any    |       No         |
///
/// A `Runnable` is a reference to a task that has been scheduled. There can be
/// at most one `Runnable` at any given time.
///
/// `WAKE_COUNT` is a counter incremented each time the task is awaken and reset
/// each time the `Runnable` has finished polling the task. The waker that
/// increments the wake count from 0 to 1 is responsible for creating and
/// scheduling a new `Runnable`.
///
/// The state includes as well a reference count `REF_COUNT` that accounts for
/// the `Promise`, the `CancelToken` and all `Waker`s. The `Runnable` is _not_
/// included in `REF_COUNT` because its existence can be inferred from `CLOSED`,
/// `POLLING` and `WAKE_COUNT` (see table above).
struct Task<F: Future, S, T> {
    /// State of the task.
    ///
    /// The state has the following layout, where bit 0 is the LSB and bit 63 is
    /// the MSB:
    ///
    /// |    33-63   |    2-32   |    1   |    0    |
    /// |------------|-----------|--------|---------|
    /// | WAKE_COUNT | REF_COUNT | CLOSED | POLLING |
    state: AtomicU64,

    /// The future, its output, or nothing.
    core: UnsafeCell<TaskCore<F>>,

    /// The task scheduling function.
    schedule_fn: S,

    /// An arbitrary `Clone` tag that is passed to the scheduling function.
    tag: T,
}

impl<F, S, T> Task<F, S, T>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    /// Clones a waker.
    unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
        let this = &*(ptr as *const Self);

        let ref_count = this.state.fetch_add(REF_INC, Ordering::Relaxed) & REF_MASK;
        if ref_count > REF_CRITICAL {
            panic!("Attack of the clones: the waker was cloned too many times");
        }

        RawWaker::new(ptr, raw_waker_vtable::<F, S, T>())
    }

    /// Wakes the task by value.
    unsafe fn wake_by_val(ptr: *const ()) {
        // Verify that the scheduling function does not capture any variable.
        //
        // It is always possible for the `Runnable` scheduled in the call to
        // `wake` to be called and complete its execution before the scheduling
        // call returns. For efficiency reasons, the reference count is
        // preemptively decremented, which implies that the `Runnable` could
        // prematurely drop and deallocate this task. By making sure that the
        // schedule function is zero-sized, we ensure that premature
        // deallocation is safe since the scheduling function does not access
        // any allocated data.
        if mem::size_of::<S>() != 0 {
            // Note: a static assert is not possible as `S` is defined in the
            // outer scope.
            Self::drop_waker(ptr);
            panic!("Scheduling functions with captured variables are not supported");
        }

        // Wake the task, decreasing at the same time the reference count.
        let state = Self::wake(ptr, WAKE_INC - REF_INC);

        // Deallocate the task if this waker is the last reference to the task,
        // meaning that the reference count was 1 and the `POLLING` flag was
        // cleared. Note that if the `POLLING` flag was set then a `Runnable`
        // must exist.

        if state & (REF_MASK | POLLING) == REF_INC {
            // Ensure that the newest state of the task output (if any) is
            // visible before it is dropped.
            //
            // Ordering: Acquire ordering is necessary to synchronize with the
            // Release ordering in all previous reference count decrements
            // and/or in the wake count reset (the latter is equivalent to a
            // reference count decrement for a `Runnable`).
            atomic::fence(Ordering::Acquire);

            let this = &*(ptr as *const Self);

            // Set a drop guard to ensure that the task is deallocated whether
            // or not `output` panics when dropped.
            let _drop_guard = RunOnDrop::new(|| {
                dealloc(ptr as *mut u8, Layout::new::<Self>());
            });

            if state & CLOSED == 0 {
                // Since the `CLOSED` and `POLLING` flags are both cleared, the
                // output is present and must be dropped.
                this.core.with_mut(|c| ManuallyDrop::drop(&mut (*c).output));
            }
            // Else the `CLOSED` flag is set and the `POLLING` flag is cleared
            // so the task is already in the `Closed` phase.
        }
    }

    /// Wakes the task by reference.
    unsafe fn wake_by_ref(ptr: *const ()) {
        // Wake the task.
        Self::wake(ptr, WAKE_INC);
    }

    /// Wakes the task, either by value or by reference.
    #[inline(always)]
    unsafe fn wake(ptr: *const (), state_delta: u64) -> u64 {
        let this = &*(ptr as *const Self);

        // Increment the wake count and, if woken by value, decrement the
        // reference count at the same time.
        //
        // Ordering: Release ordering is necessary to synchronize with either
        // the Acquire load or with the RMW in `Runnable::run`, which ensures
        // that all memory operations performed by the user before the call to
        // `wake` will be visible when the future is polled. Note that there is
        // no need to use AcqRel ordering to synchronize with all calls to
        // `wake` that precede the call to `Runnable::run`. This is because,
        // according to the C++ memory model, an RMW takes part in a Release
        // sequence irrespective of its ordering. The below RMW also happens to
        // takes part in another Release sequence: it allows the Acquire-Release
        // RMW that zeroes the wake count in the previous call to
        // `Runnable::run` to synchronizes with the initial Acquire load of the
        // state in the next call `Runnable::run` (or the Acquire fence in
        // `Runnable::cancel`), thus ensuring that the next `Runnable` sees the
        // newest state of the future.
        let state = this.state.fetch_add(state_delta, Ordering::Release);

        if state & WAKE_MASK > WAKE_CRITICAL {
            panic!("The task was woken too many times: {:0x}", state);
        }

        // Schedule the task if it is in the `Polling` phase but is not
        // scheduled yet.
        if state & (WAKE_MASK | CLOSED | POLLING) == POLLING {
            // Safety: calling `new_unchecked` is safe since: there is no other
            // `Runnable` running (the wake count was 0, the `POLLING` flag was
            // set, the `CLOSED` flag was cleared); the wake count is now 1; the
            // `POLLING` flag is set; the `CLOSED` flag is cleared; the task
            // contains a live future.

            let runnable = Runnable::new_unchecked(ptr as *const Self);
            (this.schedule_fn)(runnable, this.tag.clone());
        }

        state
    }

    /// Drops a waker.
    unsafe fn drop_waker(ptr: *const ()) {
        let this = &*(ptr as *const Self);

        // Ordering: Release ordering is necessary to synchronize with the
        // Acquire fence in the drop handler of the last reference to the task
        // and to make sure that all previous operations on the `core` member
        // are visible when it is dropped.
        let state = this.state.fetch_sub(REF_INC, Ordering::Release);

        // Deallocate the task if this waker was the last reference to the task.
        if state & REF_MASK == REF_INC && !runnable_exists(state) {
            // Ensure that the newest state of the `core` member is visible
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
                dealloc(ptr as *mut u8, Layout::new::<Self>());
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
}

/// Returns a reference to the waker's virtual table.
///
/// Unfortunately, Rust will sometimes create multiple memory instances of the
/// virtual table for the same generic parameters, which defeats
/// `Waker::will_wake` as the latter tests the pointers to the virtual tables
/// for equality.
///
/// Preventing the function from being inlined appears to solve this problem,
/// but we may want to investigate more robust methods. For unrelated reasons,
/// Tokio has switched [1] to a single non-generic virtual table declared as
/// `static` which then delegates each call to another virtual call. This does
/// ensure that `Waker::will_wake` will always work, but the double indirection
/// is a bit unfortunate and its cost would need to be evaluated.
///
/// [1]: https://github.com/tokio-rs/tokio/pull/5213
#[inline(never)]
fn raw_waker_vtable<F, S, T>() -> &'static RawWakerVTable
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    &RawWakerVTable::new(
        Task::<F, S, T>::clone_waker,
        Task::<F, S, T>::wake_by_val,
        Task::<F, S, T>::wake_by_ref,
        Task::<F, S, T>::drop_waker,
    )
}

/// Spawns a task.
///
/// An arbitrary tag can be attached to the task, a clone of which will be
/// passed to the scheduling function each time it is called.
///
/// The returned `Runnable` must be scheduled by the user.
pub(crate) fn spawn<F, S, T>(
    future: F,
    schedule_fn: S,
    tag: T,
) -> (Promise<F::Output>, Runnable, CancelToken)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    // Create a task with preemptively incremented reference and wake counts to
    // account for the returned `Promise`, `CancelToken` and `Runnable` (a
    // non-zero wake count with the `POLLING` flag set indicates that there is a
    // live `Runnable`).
    let task = Task {
        state: AtomicU64::new((2 * REF_INC) | WAKE_INC | POLLING),
        core: UnsafeCell::new(TaskCore {
            future: ManuallyDrop::new(future),
        }),
        schedule_fn,
        tag,
    };

    // Pin the task with its future to the heap.
    unsafe {
        let layout = Layout::new::<Task<F, S, T>>();
        let ptr = alloc(layout) as *mut Task<F, S, T>;
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        *ptr = task;

        // Safety: this is safe since the task was allocated with the global
        // allocator, there is no other `Runnable` running since the task was
        // just created, the wake count is 1, the `POLLING` flag is set, the
        // `CLOSED` flag is cleared and `core` contains a future.
        let runnable = Runnable::new_unchecked(ptr);

        // Safety: this is safe since the task was allocated with the global
        // allocator and the reference count is 2.
        let promise = Promise::new_unchecked(ptr);
        let cancel_token = CancelToken::new_unchecked(ptr);

        (promise, runnable, cancel_token)
    }
}

/// Spawns a task which output will never be retrieved.
///
/// This is mostly useful to avoid undue reference counting for futures that
/// return a `()` type.
///
/// An arbitrary tag can be attached to the task, a clone of which will be
/// passed to the scheduling function each time it is called.
///
/// The returned `Runnable` must be scheduled by the user.
pub(crate) fn spawn_and_forget<F, S, T>(
    future: F,
    schedule_fn: S,
    tag: T,
) -> (Runnable, CancelToken)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Fn(Runnable, T) + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    // Create a task with preemptively incremented reference and wake counts to
    // account for the returned `CancelToken` and `Runnable` (a non-zero wake
    // count with the `POLLING` flag set indicates that there is a live
    // `Runnable`).
    let task = Task {
        state: AtomicU64::new(REF_INC | WAKE_INC | POLLING),
        core: UnsafeCell::new(TaskCore {
            future: ManuallyDrop::new(future),
        }),
        schedule_fn,
        tag,
    };

    // Pin the task with its future to the heap.
    unsafe {
        let layout = Layout::new::<Task<F, S, T>>();
        let ptr = alloc(layout) as *mut Task<F, S, T>;
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        *ptr = task;

        // Safety: this is safe since the task was allocated with the global
        // allocator, there is no other `Runnable` running since the task was
        // just created, the wake count is 1, the `POLLING` flag is set, the
        // `CLOSED` flag is cleared and `core` contains a future.
        let runnable = Runnable::new_unchecked(ptr);

        // Safety: this is safe since the task was allocated with the global
        // allocator and the reference count is 1.
        let cancel_token = CancelToken::new_unchecked(ptr);

        (runnable, cancel_token)
    }
}
