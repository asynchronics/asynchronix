//! Multi-threaded `async` executor.
//!
//! The executor is exclusively designed for message-passing computational
//! tasks. As such, it does not include an I/O reactor and does not consider
//! fairness as a goal in itself. While it does use fair local queues inasmuch
//! as these tend to perform better in message-passing applications, it uses an
//! unfair injection queue and a LIFO slot without attempt to mitigate the
//! effect of badly behaving code (e.g. futures that spin-lock by yielding to
//! the executor; there is for this reason no support for something like tokio's
//! `yield_now`).
//!
//! Another way in which it differs from other `async` executors is that it
//! treats deadlocking as a normal occurrence. This is because in a
//! discrete-time simulator, the simulation of a system at a given time step
//! will make as much progress as possible until it technically reaches a
//! deadlock. Only then does the simulator advance the simulated time until the
//! next "event" extracted from a time-sorted priority queue.
//!
//! The design of the executor is largely influenced by the tokio and go
//! schedulers, both of which are optimized for message-passing applications. In
//! particular, it uses fast, fixed-size thread-local work-stealing queues with
//! a non-stealable LIFO slot in combination with an injector queue, which
//! injector queue is used both to schedule new tasks and to absorb temporary
//! overflow in the local queues.
//!
//! The design of the injector queue is kept very simple compared to tokio, by
//! taking advantage of the fact that the injector is not required to be either
//! LIFO or FIFO. Moving tasks between a local queue and the injector is fast
//! because tasks are moved in batch and are stored contiguously in memory.
//!
//! Another difference with tokio is that, at the moment, the complete subset of
//! active worker threads is stored in a single atomic variable. This makes it
//! possible to rapidly identify free worker threads for stealing operations,
//! with the downside that the maximum number of worker threads is currently
//! limited to `usize::BITS`. This is unlikely to constitute a limitation in
//! practice though since system simulation is not typically embarrassingly
//! parallel.
//!
//! Probably the largest difference with tokio is the task system, which has
//! better throughput due to less need for synchronization. This mainly results
//! from the use of an atomic notification counter rather than an atomic
//! notification flag, thus alleviating the need to reset the notification flag
//! before polling a future.

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_utils::sync::{Parker, Unparker};
use slab::Slab;

mod find_bit;
mod injector;
mod pool_manager;
mod queue;
mod rng;
mod task;
mod worker;

#[cfg(all(test, not(asynchronix_loom)))]
mod tests;

use self::pool_manager::PoolManager;
use self::rng::Rng;
use self::task::{CancelToken, Promise, Runnable};
use self::worker::Worker;
use crate::macros::scoped_local_key::scoped_thread_local;

type Bucket = injector::Bucket<Runnable, 128>;
type Injector = injector::Injector<Runnable, 128>;
type LocalQueue = queue::Worker<Runnable, queue::B256>;
type Stealer = queue::Stealer<Runnable, queue::B256>;

scoped_thread_local!(static LOCAL_WORKER: Worker);
scoped_thread_local!(static ACTIVE_TASKS: Mutex<Slab<CancelToken>>);
static NEXT_EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

/// A multi-threaded `async` executor.
#[derive(Debug)]
pub(crate) struct Executor {
    /// Shared executor data.
    context: Arc<ExecutorContext>,
    /// List of tasks that have not completed yet.
    active_tasks: Arc<Mutex<Slab<CancelToken>>>,
    /// Parker for the main executor thread.
    parker: Parker,
    /// Join handles of the worker threads.
    worker_handles: Vec<JoinHandle<()>>,
}

impl Executor {
    /// Creates an executor that runs futures on a thread pool.
    ///
    /// The maximum number of threads is set with the `num_threads` parameter.
    pub(crate) fn new(num_threads: usize) -> Self {
        let parker = Parker::new();
        let unparker = parker.unparker().clone();

        let (local_data, shared_data): (Vec<_>, Vec<_>) = (0..num_threads)
            .map(|_| {
                let parker = Parker::new();
                let unparker = parker.unparker().clone();
                let local_queue = LocalQueue::new();
                let stealer = local_queue.stealer();

                ((local_queue, parker), (stealer, unparker))
            })
            .unzip();

        // Each executor instance has a unique ID inherited by tasks to ensure
        // that tasks are scheduled on their parent executor.
        let executor_id = NEXT_EXECUTOR_ID.fetch_add(1, Ordering::Relaxed);
        assert!(
            executor_id <= usize::MAX / 2,
            "{} executors have been instantiated: this is most probably a bug.",
            usize::MAX / 2
        );

        let context = Arc::new(ExecutorContext::new(
            executor_id,
            unparker,
            shared_data.into_iter(),
        ));
        let active_tasks = Arc::new(Mutex::new(Slab::new()));

        // All workers must be marked as active _before_ spawning the threads to
        // make sure that the count of active workers does not fall to zero
        // before all workers are blocked on the signal barrier.
        context.pool_manager.set_all_workers_active();

        // Spawn all worker threads.
        let worker_handles: Vec<_> = local_data
            .into_iter()
            .enumerate()
            .into_iter()
            .map(|(id, (local_queue, worker_parker))| {
                let thread_builder = thread::Builder::new().name(format!("Worker #{}", id));

                thread_builder
                    .spawn({
                        let context = context.clone();
                        let active_tasks = active_tasks.clone();
                        move || {
                            let worker = Worker::new(local_queue, context);
                            ACTIVE_TASKS.set(&active_tasks, || {
                                LOCAL_WORKER
                                    .set(&worker, || run_local_worker(&worker, id, worker_parker))
                            });
                        }
                    })
                    .unwrap()
            })
            .collect();

        // Wait until all workers are blocked on the signal barrier.
        parker.park();
        assert!(context.pool_manager.pool_is_idle());

        Self {
            context,
            active_tasks,
            parker,
            worker_handles,
        }
    }

    /// Spawns a task and returns a promise that can be polled to retrieve the
    /// task's output.
    pub(crate) fn spawn<T>(&self, future: T) -> Promise<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        // Book a slot to store the task cancellation token.
        let mut active_tasks = self.active_tasks.lock().unwrap();
        let task_entry = active_tasks.vacant_entry();

        // Wrap the future so that it removes its cancel token from the
        // executor's list when dropped.
        let future = CancellableFuture::new(future, task_entry.key());

        let (promise, runnable, cancel_token) =
            task::spawn(future, schedule_task, self.context.executor_id);

        task_entry.insert(cancel_token);
        self.context.injector.insert_task(runnable);

        self.context.pool_manager.activate_worker();

        promise
    }

    /// Spawns a task which output will never be retrieved.
    ///
    /// This is mostly useful to avoid undue reference counting for futures that
    /// return a `()` type.
    pub(crate) fn spawn_and_forget<T>(&self, future: T)
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        // Book a slot to store the task cancellation token.
        let mut active_tasks = self.active_tasks.lock().unwrap();
        let task_entry = active_tasks.vacant_entry();

        // Wrap the future so that it removes its cancel token from the
        // executor's list when dropped.
        let future = CancellableFuture::new(future, task_entry.key());

        let (runnable, cancel_token) =
            task::spawn_and_forget(future, schedule_task, self.context.executor_id);

        task_entry.insert(cancel_token);
        self.context.injector.insert_task(runnable);

        self.context.pool_manager.activate_worker();
    }

    /// Let the executor run, blocking until all futures have completed or until
    /// the executor deadlocks.
    pub(crate) fn run(&mut self) {
        loop {
            if let Some(worker_panic) = self.context.pool_manager.take_panic() {
                panic::resume_unwind(worker_panic);
            }
            if self.context.pool_manager.pool_is_idle() {
                return;
            }

            self.parker.park();
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Force all threads to return.
        self.context.pool_manager.trigger_termination();
        for handle in self.worker_handles.drain(0..) {
            handle.join().unwrap();
        }

        // Drop all tasks that have not completed.
        //
        // A local worker must be set because some tasks may schedule other
        // tasks when dropped, which requires that a local worker be available.
        let worker = Worker::new(LocalQueue::new(), self.context.clone());
        LOCAL_WORKER.set(&worker, || {
            // Cancel all pending futures.
            //
            // `ACTIVE_TASKS` is explicitly unset to prevent
            // `CancellableFuture::drop()` from trying to remove its own token
            // from the list of active tasks as this would result in a reentrant
            // lock. This is mainly to stay on the safe side: `ACTIVE_TASKS`
            // should not be set on this thread anyway, unless for some reason
            // the executor runs inside another executor.
            ACTIVE_TASKS.unset(|| {
                let mut tasks = self.active_tasks.lock().unwrap();
                for task in tasks.drain() {
                    task.cancel();
                }

                // Some of the dropped tasks may have scheduled other tasks that
                // were not yet cancelled, preventing them from being dropped
                // upon cancellation. This is OK: the scheduled tasks will be
                // dropped when the local and injector queues are dropped, and
                // they cannot re-schedule one another since all tasks were
                // cancelled.
            });
        });
    }
}

/// Shared executor context.
///
/// This contains all executor resources that can be shared between threads.
#[derive(Debug)]
struct ExecutorContext {
    /// Injector queue.
    injector: Injector,
    /// Unique executor ID inherited by all tasks spawned on this executor instance.
    executor_id: usize,
    /// Unparker for the main executor thread.
    executor_unparker: Unparker,
    /// Manager for all worker threads.
    pool_manager: PoolManager,
}

impl ExecutorContext {
    /// Creates a new shared executor context.
    pub(super) fn new(
        executor_id: usize,
        executor_unparker: Unparker,
        shared_data: impl Iterator<Item = (Stealer, Unparker)>,
    ) -> Self {
        let (stealers, worker_unparkers): (Vec<_>, Vec<_>) = shared_data.into_iter().unzip();
        let worker_unparkers = worker_unparkers.into_boxed_slice();

        Self {
            injector: Injector::new(),
            executor_id,
            executor_unparker,
            pool_manager: PoolManager::new(
                worker_unparkers.len(),
                stealers.into_boxed_slice(),
                worker_unparkers,
            ),
        }
    }
}

/// A `Future` wrapper that removes its cancellation token from the list of
/// active tasks when dropped.
struct CancellableFuture<T: Future> {
    inner: T,
    cancellation_key: usize,
}

impl<T: Future> CancellableFuture<T> {
    /// Creates a new `CancellableFuture`.
    fn new(fut: T, cancellation_key: usize) -> Self {
        Self {
            inner: fut,
            cancellation_key,
        }
    }
}

impl<T: Future> Future for CancellableFuture<T> {
    type Output = T::Output;

    #[inline(always)]
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll(cx) }
    }
}

impl<T: Future> Drop for CancellableFuture<T> {
    fn drop(&mut self) {
        // Remove the task from the list of active tasks if the future is
        // dropped on a worker thread. Otherwise do nothing and let the
        // executor's drop handler do the cleanup.
        let _ = ACTIVE_TASKS.map(|active_tasks| {
            // Don't unwrap on `lock()` because this function can be called from
            // a destructor and should not panic. In the worse case, the cancel
            // token will be left in the list of active tasks, which does
            // prevents eager task deallocation but does not cause any issue
            // otherwise.
            if let Ok(mut active_tasks) = active_tasks.lock() {
                let _cancel_token = active_tasks.try_remove(self.cancellation_key);
            }
        });
    }
}

/// Schedules a `Runnable` from within a worker thread.
///
/// # Panics
///
/// This function will panic if called from a non-worker thread or if called
/// from the worker thread of another executor instance than the one the task
/// for this `Runnable` was spawned on.
fn schedule_task(task: Runnable, executor_id: usize) {
    LOCAL_WORKER
        .map(|worker| {
            let pool_manager = &worker.executor_context.pool_manager;
            let injector = &worker.executor_context.injector;
            let local_queue = &worker.local_queue;
            let fast_slot = &worker.fast_slot;

            // Check that this task was indeed spawned on this executor.
            assert_eq!(
                executor_id, worker.executor_context.executor_id,
                "Tasks must be awaken on the same executor they are spawned on"
            );

            // Store the task in the fast slot and retrieve the one that was
            // formerly stored, if any.
            let prev_task = match fast_slot.replace(Some(task)) {
                // If there already was a task in the slot, proceed so it can be
                // moved to a task queue.
                Some(t) => t,
                // Otherwise return immediately: this task cannot be stolen so
                // there is no point in activating a sibling worker.
                None => return,
            };

            // Push the previous task to the local queue if possible or on the
            // injector queue otherwise.
            if let Err(prev_task) = local_queue.push(prev_task) {
                // The local queue is full. Try to move half of it to the
                // injector queue; if this fails, just push one task to the
                // injector queue.
                if let Ok(drain) = local_queue.drain(|_| Bucket::capacity()) {
                    injector.push_bucket(Bucket::from_iter(drain));
                    local_queue.push(prev_task).unwrap();
                } else {
                    injector.insert_task(prev_task);
                }
            }

            // A task has been pushed to the local or injector queue: try to
            // activate another worker if no worker is currently searching for a
            // task.
            if pool_manager.searching_worker_count() == 0 {
                pool_manager.activate_worker_relaxed();
            }
        })
        .expect("Tasks may not be awaken outside executor threads");
}

/// Processes all incoming tasks on a worker thread until the `Terminate` signal
/// is received or until it panics.
///
/// Panics caught in this thread are relayed to the main executor thread.
fn run_local_worker(worker: &Worker, id: usize, parker: Parker) {
    let pool_manager = &worker.executor_context.pool_manager;
    let injector = &worker.executor_context.injector;
    let executor_unparker = &worker.executor_context.executor_unparker;
    let local_queue = &worker.local_queue;
    let fast_slot = &worker.fast_slot;

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // Set how long to spin when searching for a task.
        const MAX_SEARCH_DURATION: Duration = Duration::from_nanos(1000);

        // Seed a thread RNG with the worker ID.
        let rng = Rng::new(id as u64);

        loop {
            // Signal barrier: park until notified to continue or terminate.

            // Try to deactivate the worker.
            if pool_manager.try_set_worker_inactive(id) {
                parker.park();
                // No need to call `begin_worker_search()`: this was done by the
                // thread that unparked the worker.
            } else if injector.is_empty() {
                // This worker could not be deactivated because it was the last
                // active worker. In such case, the call to
                // `try_set_worker_inactive` establishes a synchronization with
                // all threads that pushed tasks to the injector queue but could
                // not activate a new worker, which is why some tasks may now be
                // visible in the injector queue.
                pool_manager.set_all_workers_inactive();
                executor_unparker.unpark();
                parker.park();
                // No need to call `begin_worker_search()`: this was done by the
                // thread that unparked the worker.
            } else {
                pool_manager.begin_worker_search();
            }

            if pool_manager.termination_is_triggered() {
                return;
            }

            let mut search_start = Instant::now();

            // Process the tasks one by one.
            loop {
                // Check the injector queue first.
                if let Some(bucket) = injector.pop_bucket() {
                    let bucket_iter = bucket.into_iter();

                    // There is a _very_ remote possibility that, even though
                    // the local queue is empty, it has temporarily too little
                    // spare capacity for the bucket. This could happen if a
                    // concurrent steal operation was preempted for all the time
                    // it took to pop and process the remaining tasks and it
                    // hasn't released the stolen capacity yet.
                    //
                    // Unfortunately, we cannot just skip checking the injector
                    // queue altogether when there isn't enough spare capacity
                    // in the local queue because this could lead to a race:
                    // suppose that (1) this thread has earlier pushed tasks
                    // onto the injector queue, and (2) the stealer has
                    // processed all stolen tasks before this thread sees the
                    // capacity restored and at the same time (3) the stealer
                    // does not yet see the tasks this thread pushed to the
                    // injector queue; in such scenario, both this thread and
                    // the stealer thread may park and leave unprocessed tasks
                    // in the injector queue.
                    //
                    // This is the only instance where spinning is used, as the
                    // probability of this happening is close to zero and the
                    // complexity of a signaling mechanism (condvar & friends)
                    // wouldn't carry its weight.
                    while local_queue.spare_capacity() < bucket_iter.len() {}

                    // Since empty buckets are never pushed onto the injector
                    // queue, we should now have at least one task to process.
                    local_queue.extend(bucket_iter);
                } else {
                    // The injector queue is empty. Try to steal from active
                    // siblings.
                    let mut stealers = pool_manager.shuffled_stealers(Some(id), &rng);
                    if stealers.all(|stealer| {
                        stealer
                            .steal_and_pop(local_queue, |n| n - n / 2)
                            .map(|task| {
                                let prev_task = fast_slot.replace(Some(task));
                                assert!(prev_task.is_none());
                            })
                            .is_err()
                    }) {
                        // Give up if unsuccessful for too long.
                        if (Instant::now() - search_start) > MAX_SEARCH_DURATION {
                            pool_manager.end_worker_search();
                            break;
                        }

                        // Re-try.
                        continue;
                    }
                }

                // Signal the end of the search so that another worker can be
                // activated when a new task is scheduled.
                pool_manager.end_worker_search();

                // Pop tasks from the fast slot or the local queue.
                while let Some(task) = fast_slot.take().or_else(|| local_queue.pop()) {
                    if pool_manager.termination_is_triggered() {
                        return;
                    }
                    task.run();
                }

                // Resume the search for tasks.
                pool_manager.begin_worker_search();
                search_start = Instant::now();
            }
        }
    }));

    // Propagate the panic, if any.
    if let Err(panic) = result {
        pool_manager.register_panic(panic);
        pool_manager.trigger_termination();
        executor_unparker.unpark();
    }
}
