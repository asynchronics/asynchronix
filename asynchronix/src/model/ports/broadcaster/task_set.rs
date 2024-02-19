use std::sync::atomic::Ordering;
use std::sync::Arc;

use diatomic_waker::WakeSource;
use futures_task::{waker_ref, ArcWake, WakerRef};

use crate::loom_exports::sync::atomic::{AtomicU32, AtomicU64};

/// Special value for the `next` field of a task, indicating that the task to
/// which this field belongs is not currently in the list of scheduled tasks.
const SLEEPING: u32 = u32::MAX;
/// Special value for a task index, indicating the absence of task.
const EMPTY: u32 = u32::MAX - 1;
/// Mask for the index of the task pointed to by the head of the list of
/// scheduled tasks.
const INDEX_MASK: u64 = u32::MAX as u64;
/// Mask for the scheduling countdown in the head of the list of scheduled
/// tasks.
const COUNTDOWN_MASK: u64 = !INDEX_MASK;
/// A single increment of the scheduling countdown in the head of the list of
/// scheduled tasks.
const COUNTDOWN_ONE: u64 = 1 << 32;

/// An object for the efficient management of a set of tasks scheduled
/// concurrently.
///
/// The algorithm used by `TaskSet` is designed to wake up the parent task as
/// seldom as possible, ideally only when all non-completed sub-tasks have been
/// scheduled (awaken).
///
/// A `TaskSet` maintains both a vector-based list of tasks (or more accurately,
/// task waker handles) and a linked list of the subset of tasks that are
/// currently scheduled. The latter is stored in a vector-based Treiber stack
/// which links tasks through indices rather than pointers. Using indices has
/// two advantages: (i) it makes a fully safe implementation possible and (ii)
/// it can take advantage of a single CAS to simultaneously move the head and
/// decrement the outstanding amount of tasks to be scheduled before the parent
/// task is notified.
pub(super) struct TaskSet {
    /// Set of all tasks, scheduled or not.
    ///
    /// In some cases, the use of `resize()` to shrink the task set may leave
    /// inactive tasks at the back of the vector, in which case the length of
    /// the vector will exceed `task_count`.
    tasks: Vec<Arc<Task>>,
    /// Head of the Treiber stack for scheduled tasks.
    ///
    /// The lower 32 bits specify the index of the last scheduled task (head),
    /// if any, whereas the upper 32 bits specify the countdown of tasks still
    /// to be scheduled before the parent task is notified.
    head: Arc<AtomicU64>,
    /// A notifier used to wake the parent task.
    notifier: WakeSource,
    /// Count of all tasks, scheduled or not.
    task_count: usize,
}

impl TaskSet {
    /// Creates an initially empty set of tasks associated to the parent task
    /// which notifier is provided.
    #[allow(clippy::assertions_on_constants)]
    pub(super) fn new(notifier: WakeSource) -> Self {
        // Only 32-bit targets and above are supported.
        assert!(usize::BITS >= u32::BITS);

        Self {
            tasks: Vec::new(),
            head: Arc::new(AtomicU64::new(EMPTY as u64)),
            notifier,
            task_count: 0,
        }
    }

    /// Take all scheduled tasks and returns an iterator over their indices, or
    /// if there are no currently scheduled tasks returns `None` and requests a
    /// notification to be sent after `pending_task_count` tasks have been
    /// scheduled.
    ///
    /// In all cases, the list of scheduled tasks will be empty right after this
    /// call.
    ///
    /// If there were scheduled tasks, no notification is requested because this
    /// method is expected to be called repeatedly until it returns `None`.
    /// Failure to do so will result in missed notifications.
    ///
    /// If no tasks were scheduled, the notification is guaranteed to be
    /// triggered no later than after `pending_task_count` tasks have been
    /// scheduled, though it may in some cases be triggered earlier. If the
    /// specified `pending_task_count` is zero then no notification is
    /// requested.
    pub(super) fn take_scheduled(&self, pending_task_count: usize) -> Option<TaskIterator<'_>> {
        let countdown = u32::try_from(pending_task_count).unwrap();

        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let new_head = if head & INDEX_MASK == EMPTY as u64 {
                (countdown as u64 * COUNTDOWN_ONE) | EMPTY as u64
            } else {
                EMPTY as u64
            };

            // Ordering: this Acquire operation synchronizes with all Release
            // operations in `Task::wake_by_ref` and ensures that all memory
            // operations performed during and before the tasks were scheduled
            // become visible.
            match self.head.compare_exchange_weak(
                head,
                new_head,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(h) => head = h,
            }
        }

        let index = (head & INDEX_MASK) as u32;
        if index == EMPTY {
            None
        } else {
            Some(TaskIterator {
                task_list: self,
                next_index: index,
            })
        }
    }

    /// Discards all scheduled tasks and cancels any request for notification
    /// that may be set.
    ///
    /// This method is very cheap if there are no scheduled tasks and if no
    /// notification is currently requested.
    ///
    /// All discarded tasks are put in the sleeping (unscheduled) state.
    pub(super) fn discard_scheduled(&self) {
        if self.head.load(Ordering::Relaxed) != EMPTY as u64 {
            // Dropping the iterator ensures that all tasks are put in the
            // sleeping state.
            let _ = self.take_scheduled(0);
        }
    }

    /// Set the number of active tasks.
    ///
    /// Note that this method may discard already scheduled tasks.
    ///
    /// # Panic
    ///
    /// This method will panic if `len` is greater than `u32::MAX - 1`.
    pub(super) fn resize(&mut self, len: usize) {
        assert!(len <= EMPTY as usize && len <= SLEEPING as usize);

        self.task_count = len;

        // Add new tasks if necessary.
        if len >= self.tasks.len() {
            while len > self.tasks.len() {
                let idx = self.tasks.len() as u32;

                self.tasks.push(Arc::new(Task {
                    idx,
                    notifier: self.notifier.clone(),
                    next: AtomicU32::new(SLEEPING),
                    head: self.head.clone(),
                }));
            }

            return;
        }

        // Try to remove inactive tasks.
        //
        // The main issue when shrinking the set of active tasks is that stale
        // wakers may still be around and may at any moment be scheduled and
        // insert their index in the list of scheduled tasks. If it cannot be
        // guaranteed that this will not happen, then a reference to that task
        // must be kept or the iterator for scheduled tasks will panic when
        // indexing a stale task.
        //
        // To prevent an inactive task from being spuriously scheduled, it is
        // enough to pretend that the task is already scheduled by setting its
        // `next` field to anything else than `SLEEPING`. However, this could
        // race if the task has just set its `next` field but has not yet
        // updated the head of the list of scheduled tasks, so this can only be
        // done reliably if the task is currently sleeping.

        // All scheduled tasks are first unscheduled in case some of them are
        // now inactive.
        self.discard_scheduled();

        // The position of tasks in the set must stay consistent with their
        // associated index so tasks are popped from the back.
        while self.tasks.len() > len {
            // There is at least one task since `len()` was non-zero.
            let task = self.tasks.last().unwrap();

            // Ordering: Relaxed ordering is sufficient since the task is
            // effectively discarded.
            if task
                .next
                .compare_exchange(SLEEPING, EMPTY, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                // The task could not be removed for now so the set of tasks cannot
                // be shrunk further.
                break;
            }

            self.tasks.pop();
        }
    }

    /// Returns `true` if one or more sub-tasks are currently scheduled.
    pub(super) fn has_scheduled(&self) -> bool {
        // Ordering: the content of the head is only used as an advisory flag so
        // Relaxed ordering is sufficient.
        self.head.load(Ordering::Relaxed) & INDEX_MASK != EMPTY as u64
    }

    /// Returns a reference to the waker associated to the active task with the
    /// specified index.
    ///
    /// # Panics
    ///
    /// This method will panic if there is no active task with the provided
    /// index.
    pub(super) fn waker_of(&self, idx: usize) -> WakerRef {
        assert!(idx < self.task_count);

        waker_ref(&self.tasks[idx])
    }
}

/// An asynchronous task associated with the future of a sender.
pub(super) struct Task {
    /// Index of this task.
    idx: u32,
    /// A notifier triggered once a certain number of tasks have been scheduled.
    notifier: WakeSource,
    /// Index of the next task in the list of scheduled tasks.
    next: AtomicU32,
    /// Head of the list of scheduled tasks.
    head: Arc<AtomicU64>,
}

impl ArcWake for Task {
    fn wake(self: Arc<Self>) {
        Self::wake_by_ref(&self);
    }
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let mut next = arc_self.next.load(Ordering::Relaxed);

        let mut head = loop {
            if next == SLEEPING {
                // The task appears not to be scheduled yet: prepare its
                // insertion in the list of scheduled tasks by setting the next
                // task index to the index of the task currently pointed by the
                // head.
                //
                // Ordering: Relaxed ordering is sufficient since the upcoming
                // CAS on the head already ensure that all memory operations
                // that precede this call to `wake_by_ref` become visible when
                // the tasks are stolen.
                let head = arc_self.head.load(Ordering::Relaxed);
                match arc_self.next.compare_exchange_weak(
                    SLEEPING,
                    (head & INDEX_MASK) as u32,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break head,
                    Err(n) => next = n,
                }
            } else {
                // The task appears to be already scheduled: confirm this and
                // establish proper memory synchronization by performing a no-op
                // RMW.
                //
                // Ordering: the Release ordering synchronizes with the Acquire
                // swap operation in `TaskIterator::next` and ensures that all
                // memory operations that precede this call to `wake_by_ref`
                // will be visible when the task index is yielded.
                match arc_self.next.compare_exchange_weak(
                    next,
                    next,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(n) => next = n,
                }
            }
        };

        // The index to the next task has been set to the index in the head.
        // Other concurrent calls to `wake` or `wake_by_ref` will now see the
        // task as scheduled so this thread is responsible for moving the head.
        loop {
            // Attempt a CAS which decrements the countdown if it is not already
            // cleared and which sets the head's index to this task's index.
            let countdown = head & COUNTDOWN_MASK;
            let new_countdown = countdown.wrapping_sub((countdown != 0) as u64 * COUNTDOWN_ONE);
            let new_head = new_countdown | arc_self.idx as u64;

            // Ordering: this Release operation synchronizes with the Acquire
            // operation on the head in `TaskSet::steal_scheduled` and ensures
            // that the value of the `next` field as well as all memory
            // operations that precede this call to `wake_by_ref` become visible
            // when the tasks are stolen.
            match arc_self.head.compare_exchange_weak(
                head,
                new_head,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // If the countdown has just been cleared, it is necessary
                    // to send a notification.
                    if countdown == COUNTDOWN_ONE {
                        arc_self.notifier.notify();
                    }

                    return;
                }
                Err(h) => {
                    head = h;

                    // Update the index of the next task to the new value of the
                    // head.
                    //
                    // Why use a swap instead of a simple store? This is to
                    // maintain a release sequence which includes previous
                    // atomic operation on this field, and more specifically any
                    // no-op CAS that could have been performed by a concurrent
                    // call to wake. This ensures in turn that all memory
                    // operations that precede a no-op CAS will be visible when
                    // `next` is Acquired in `TaskIterator::next`.
                    //
                    // Ordering: Relaxed ordering is sufficient since
                    // synchronization is ensured by the upcoming CAS on the
                    // head.
                    arc_self
                        .next
                        .swap((head & INDEX_MASK) as u32, Ordering::Relaxed);
                }
            }
        }
    }
}

/// An iterator over scheduled tasks.
pub(super) struct TaskIterator<'a> {
    task_list: &'a TaskSet,
    next_index: u32,
}

impl<'a> Iterator for TaskIterator<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index != EMPTY {
            let index = self.next_index as usize;

            // Ordering: the Acquire ordering synchronizes with any no-op CAS
            // that could have been performed in `Task::wake_by_ref`, ensuring
            // that all memory operations that precede such call to
            // `Task::wake_by_ref` become visible.
            self.next_index = self.task_list.tasks[index]
                .next
                .swap(SLEEPING, Ordering::Acquire);

            // Only yield the index if the task is indeed active.
            if index < self.task_list.task_count {
                return Some(index);
            }
        }

        None
    }
}

impl<'a> Drop for TaskIterator<'a> {
    fn drop(&mut self) {
        // Put all remaining scheduled tasks in the sleeping state.
        //
        // Ordering: the task is ignored so it is not necessary to ensure that
        // memory operations performed before the task was scheduled are
        // visible. For the same reason, it is not necessary to synchronize with
        // no-op CAS operations in `Task::wake_by_ref`, which is why separate
        // load and store operations are used rather than a more expensive swap
        // operation.
        while self.next_index != EMPTY {
            let index = self.next_index as usize;
            self.next_index = self.task_list.tasks[index].next.load(Ordering::Relaxed);
            self.task_list.tasks[index]
                .next
                .store(SLEEPING, Ordering::Relaxed);
        }
    }
}
