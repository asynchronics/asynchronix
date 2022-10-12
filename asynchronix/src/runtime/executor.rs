use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking::Parker;
use slab::Slab;

mod find_bit;
mod injector;
mod pool;
mod queue;
mod rng;
mod task;
mod worker;

#[cfg(all(test, not(asynchronix_loom)))]
mod tests;

use self::pool::{Pool, PoolState};
use self::rng::Rng;
use self::task::{CancelToken, Promise, Runnable};
use self::worker::Worker;
use crate::macros::scoped_local_key::scoped_thread_local;

type Bucket = injector::Bucket<Runnable, 128>;
type GlobalQueue = injector::Injector<Runnable, 128>;
type LocalQueue = queue::Worker<Runnable, queue::B256>;
type Stealer = queue::Stealer<Runnable, queue::B256>;

scoped_thread_local!(static LOCAL_WORKER: Worker);
scoped_thread_local!(static ACTIVE_TASKS: Mutex<Slab<CancelToken>>);
static NEXT_EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

/// A multi-threaded `async` executor.
///
/// The executor is exclusively designed for message-passing computational
/// tasks. As such, it does not include an I/O reactor and does not consider
/// fairness as a goal in itself. While it does use fair local queues inasmuch
/// as these tend to perform better in message-passing applications, it uses an
/// unfair injection queue and a LIFO slot without attempt to mitigate the
/// effect of badly behaving code (e.g. futures that use spin-locks and hope for
/// the best by yielding to the executor with something like tokio's
/// `yield_now`).
///
/// Another way in which it differs from other `async` executors is that it
/// treats deadlocking as a normal occurrence. This is because in a
/// discrete-time simulator, the simulation of a system at a given time step
/// will make as much progress as possible until it technically reaches a
/// deadlock. Only then does the simulator advance the simulated time until the
/// next "event" extracted from a time-sorted priority queue, sending it to
/// enable further progress in the computation.
///
/// The design of the executor is largely influenced by the tokio and go
/// schedulers, both of which are optimized for message-passing applications. In
/// particular, it uses fast, fixed-size thread-local work-stealing queues with
/// a "fast" non-stealable slot in combination with a global injector queue. The
/// injector queue is used both to schedule new tasks and to absorb temporary
/// overflow in the local queues. The design of the injector queue is kept very
/// simple by taking advantage of the fact that the injector is not required to
/// be either LIFO or FIFO.
///
/// Probably the largest difference with tokio is the task system, which boasts
/// a higher throughput achieved by reducing the need for synchronization.
/// Another difference is that, at the moment, the complete subset of active
/// worker threads is stored in a single atomic variable. This makes it in
/// particular possible to rapidly identify free worker threads for stealing
/// operations. The downside of this approach is that the maximum number of
/// worker threads is limited to `usize::BITS`, but this is unlikely to
/// constitute a limitation since system simulation is not typically an
/// embarrassingly parallel problem.
#[derive(Debug)]
pub(crate) struct Executor {
    pool: Arc<Pool>,
    active_tasks: Arc<Mutex<Slab<CancelToken>>>,
    parker: parking::Parker,
    join_handles: Vec<JoinHandle<()>>,
}

impl Executor {
    /// Creates an executor that runs futures on a thread pool.
    ///
    /// The maximum number of threads is set with the `num_threads` parameter.
    pub(crate) fn new(num_threads: usize) -> Self {
        let (parker, unparker) = parking::pair();

        let (local_data, shared_data): (Vec<_>, Vec<_>) = (0..num_threads)
            .map(|_| {
                let (parker, unparker) = parking::pair();
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

        let pool = Arc::new(Pool::new(executor_id, unparker, shared_data.into_iter()));
        let active_tasks = Arc::new(Mutex::new(Slab::new()));

        // All workers must be marked as active _before_ spawning the threads to
        // make sure that the count of active workers does not fall to zero
        // before all workers are blocked on the signal barrier.
        pool.set_all_workers_active();

        // Spawn all worker threads.
        let join_handles: Vec<_> = local_data
            .into_iter()
            .enumerate()
            .into_iter()
            .map(|(id, (local_queue, worker_parker))| {
                let thread_builder = thread::Builder::new().name(format!("Worker #{}", id));

                thread_builder
                    .spawn({
                        let pool = pool.clone();
                        let active_tasks = active_tasks.clone();
                        move || {
                            let worker = Worker::new(local_queue, pool);
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
        assert!(pool.is_idle());

        Self {
            pool,
            active_tasks,
            parker,
            join_handles,
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
            task::spawn(future, schedule_task, self.pool.executor_id);

        task_entry.insert(cancel_token);
        self.pool.global_queue.insert_task(runnable);

        self.pool.activate_worker();

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
            task::spawn_and_forget(future, schedule_task, self.pool.executor_id);

        task_entry.insert(cancel_token);
        self.pool.global_queue.insert_task(runnable);

        self.pool.activate_worker();
    }

    /// Let the executor run, blocking until all futures have completed or until
    /// the executor deadlocks.
    pub(crate) fn run(&mut self) {
        loop {
            if let Some(worker_panic) = self.pool.take_panic() {
                panic::resume_unwind(worker_panic);
            }
            if self.pool.is_idle() {
                return;
            }

            self.parker.park();
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Force all threads to return.
        self.pool.trigger_termination();
        for join_handle in self.join_handles.drain(0..) {
            join_handle.join().unwrap();
        }

        // Drop all tasks that have not completed.
        //
        // A local worker must be set because some tasks may schedule other
        // tasks when dropped, which requires that a local worker be available.
        let worker = Worker::new(LocalQueue::new(), self.pool.clone());
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
                // dropped when the local and global queues are dropped, and
                // they cannot re-schedule one another since all tasks were
                // cancelled.
            });
        });
    }
}

// A `Future` wrapper that removes its cancellation token from the executor's
// list of active tasks when dropped.
struct CancellableFuture<T: Future> {
    inner: T,
    cancellation_key: usize,
}
impl<T: Future> CancellableFuture<T> {
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

// Schedules a `Runnable`.
fn schedule_task(task: Runnable, executor_id: usize) {
    LOCAL_WORKER
        .map(|worker| {
            // Check that this task was indeed spawned on this executor.
            assert_eq!(
                executor_id, worker.pool.executor_id,
                "Tasks must be awaken on the same executor they are spawned on"
            );

            // Store the task in the fast slot and retrieve the one that was
            // formerly stored, if any.
            let prev_task = match worker.fast_slot.replace(Some(task)) {
                // If there already was a task in the slot, proceed so it can be
                // moved to a task queue.
                Some(t) => t,
                // Otherwise return immediately: this task cannot be stolen so
                // there is no point in activating a sibling worker.
                None => return,
            };

            // Push the previous task to the local queue if possible or on the
            // global queue otherwise.
            if let Err(prev_task) = worker.local_queue.push(prev_task) {
                // The local queue is full. Try to move half of it to the global
                // queue; if this fails, just push one task to the global queue.
                if let Ok(drain) = worker.local_queue.drain(|_| Bucket::capacity()) {
                    worker
                        .pool
                        .global_queue
                        .push_bucket(Bucket::from_iter(drain));
                    worker.local_queue.push(prev_task).unwrap();
                } else {
                    worker.pool.global_queue.insert_task(prev_task);
                }
            }

            // A task has been pushed to the local or global queue: try to
            // activate another worker if no worker is currently searching for a
            // task.
            if worker.pool.searching_worker_count() == 0 {
                worker.pool.activate_worker_relaxed();
            }
        })
        .expect("Tasks may not be awaken outside executor threads");
}

/// Processes all incoming tasks on a worker thread until the `Terminate` signal
/// is received or until it panics.
fn run_local_worker(worker: &Worker, id: usize, parker: Parker) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // Set how long to spin when searching for a task.
        const MAX_SEARCH_DURATION: Duration = Duration::from_nanos(1000);

        // Seed a thread RNG with the worker ID.
        let rng = Rng::new(id as u64);

        loop {
            // Signal barrier: park until notified to continue or terminate.
            if worker.pool.set_worker_inactive(id) == PoolState::Idle {
                // If this worker was the last active worker, it is necessary to
                // check again whether the global queue is not populated. This
                // could happen if the executor thread pushed a task to the
                // global queue but could not activate a new worker because all
                // workers were then activated.
                if !worker.pool.global_queue.is_empty() {
                    worker.pool.set_worker_active(id);
                } else {
                    worker.pool.executor_unparker.unpark();
                    parker.park();
                }
            } else {
                parker.park();
            }
            if worker.pool.termination_is_triggered() {
                return;
            }

            // We may spin for a little while: start counting.
            let mut search_start = Instant::now();

            // Process the tasks one by one.
            loop {
                // Check the global queue first.
                if let Some(bucket) = worker.pool.global_queue.pop_bucket() {
                    let bucket_iter = bucket.into_iter();

                    // There is a _very_ remote possibility that, even though
                    // the local queue is empty, it has temporarily too little
                    // spare capacity for the bucket. This could happen because
                    // a concurrent steal operation could be preempted for all
                    // the time it took to pop and process the remaining tasks
                    // and hasn't released the stolen capacity yet.
                    //
                    // Unfortunately, we cannot just skip checking the global
                    // queue altogether when there isn't enough spare capacity
                    // in the local queue, as this could lead to a race: suppose
                    // that (1) this thread has earlier pushed tasks onto the
                    // global queue, and (2) the stealer has processed all
                    // stolen tasks before this thread sees the capacity
                    // restored and at the same time (3) the stealer does not
                    // yet see the tasks this thread pushed to the global queue;
                    // in such scenario, both this thread and the stealer thread
                    // may park and leave unprocessed tasks in the global queue.
                    //
                    // This is the only instance where spinning is used, as the
                    // probability of this happening is close to zero and the
                    // complexity of a signaling mechanism (condvar & friends)
                    // wouldn't carry its weight.
                    while worker.local_queue.spare_capacity() < bucket_iter.len() {}

                    // Since empty buckets are never pushed onto the global
                    // queue, we should now have at least one task to process.
                    worker.local_queue.extend(bucket_iter);
                } else {
                    // The global queue is empty. Try to steal from active
                    // siblings.
                    let mut stealers = worker.pool.shuffled_stealers(Some(id), &rng);
                    if stealers.all(|stealer| {
                        stealer
                            .steal_and_pop(&worker.local_queue, |n| n - n / 2)
                            .map(|task| {
                                let prev_task = worker.fast_slot.replace(Some(task));
                                assert!(prev_task.is_none());
                            })
                            .is_err()
                    }) {
                        // Give up if unsuccessful for too long.
                        if (Instant::now() - search_start) > MAX_SEARCH_DURATION {
                            worker.pool.end_worker_search();
                            break;
                        }

                        // Re-try.
                        continue;
                    }
                }

                // Signal the end of the search so that another worker can be
                // activated when a new task is scheduled.
                worker.pool.end_worker_search();

                // Pop tasks from the fast slot or the local queue.
                while let Some(task) = worker.fast_slot.take().or_else(|| worker.local_queue.pop())
                {
                    if worker.pool.termination_is_triggered() {
                        return;
                    }
                    task.run();
                }

                // Resume the search for tasks.
                worker.pool.begin_worker_search();
                search_start = Instant::now();
            }
        }
    }));

    // Propagate the panic, if any.
    if let Err(panic) = result {
        worker.pool.register_panic(panic);
        worker.pool.trigger_termination();
        worker.pool.executor_unparker.unpark();
    }
}
