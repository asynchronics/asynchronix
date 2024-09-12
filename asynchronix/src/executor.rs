//! `async` executor trait.

mod mt_executor;
mod st_executor;
mod task;

use std::future::Future;
use std::sync::atomic::AtomicUsize;

use crate::macros::scoped_thread_local::scoped_thread_local;
#[cfg(feature = "tracing")]
use crate::time::AtomicTimeReader;
use task::Promise;

/// Unique identifier for executor instances.
static NEXT_EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

/// Context common to all executor types.
#[derive(Clone)]
pub(crate) struct SimulationContext {
    #[cfg(feature = "tracing")]
    pub(crate) time_reader: AtomicTimeReader,
}

scoped_thread_local!(pub(crate) static SIMULATION_CONTEXT: SimulationContext);

/// A single-threaded or multi-threaded `async` executor.
#[derive(Debug)]
pub(crate) enum Executor {
    StExecutor(st_executor::Executor),
    MtExecutor(mt_executor::Executor),
}

impl Executor {
    /// Creates an executor that runs futures on the current thread.
    pub(crate) fn new_single_threaded(simulation_context: SimulationContext) -> Self {
        Self::StExecutor(st_executor::Executor::new(simulation_context))
    }

    /// Creates an executor that runs futures on a thread pool.
    ///
    /// The maximum number of threads is set with the `num_threads` parameter.
    ///
    /// # Panics
    ///
    /// This will panic if the specified number of threads is zero or is more
    /// than `usize::BITS`.
    pub(crate) fn new_multi_threaded(
        num_threads: usize,
        simulation_context: SimulationContext,
    ) -> Self {
        Self::MtExecutor(mt_executor::Executor::new(num_threads, simulation_context))
    }

    /// Spawns a task which output will never be retrieved.
    ///
    /// Note that spawned tasks are not executed until [`run()`](Executor::run)
    /// is called.
    #[allow(unused)]
    pub(crate) fn spawn<T>(&self, future: T) -> Promise<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        match self {
            Self::StExecutor(executor) => executor.spawn(future),
            Self::MtExecutor(executor) => executor.spawn(future),
        }
    }

    /// Spawns a task which output will never be retrieved.
    ///
    /// Note that spawned tasks are not executed until [`run()`](Executor::run)
    /// is called.
    pub(crate) fn spawn_and_forget<T>(&self, future: T)
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        match self {
            Self::StExecutor(executor) => executor.spawn_and_forget(future),
            Self::MtExecutor(executor) => executor.spawn_and_forget(future),
        }
    }

    /// Execute spawned tasks, blocking until all futures have completed or
    /// until the executor reaches a deadlock.
    pub(crate) fn run(&mut self) {
        match self {
            Self::StExecutor(executor) => executor.run(),
            Self::MtExecutor(executor) => executor.run(),
        }
    }
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use futures_channel::{mpsc, oneshot};
    use futures_util::StreamExt;

    use super::*;

    fn dummy_simulation_context() -> SimulationContext {
        SimulationContext {
            #[cfg(feature = "tracing")]
            time_reader: crate::util::sync_cell::SyncCell::new(
                crate::time::TearableAtomicTime::new(crate::time::MonotonicTime::EPOCH),
            )
            .reader(),
        }
    }

    /// An object that runs an arbitrary closure when dropped.
    struct RunOnDrop<F: FnOnce()> {
        drop_fn: Option<F>,
    }
    impl<F: FnOnce()> RunOnDrop<F> {
        /// Creates a new `RunOnDrop`.
        fn new(drop_fn: F) -> Self {
            Self {
                drop_fn: Some(drop_fn),
            }
        }
    }
    impl<F: FnOnce()> Drop for RunOnDrop<F> {
        fn drop(&mut self) {
            self.drop_fn.take().map(|f| f());
        }
    }

    fn executor_deadlock(mut executor: Executor) {
        let (_sender1, receiver1) = oneshot::channel::<()>();
        let (_sender2, receiver2) = oneshot::channel::<()>();

        let launch_count = Arc::new(AtomicUsize::new(0));
        let completion_count = Arc::new(AtomicUsize::new(0));

        executor.spawn_and_forget({
            let launch_count = launch_count.clone();
            let completion_count = completion_count.clone();

            async move {
                launch_count.fetch_add(1, Ordering::Relaxed);
                let _ = receiver2.await;
                completion_count.fetch_add(1, Ordering::Relaxed);
            }
        });
        executor.spawn_and_forget({
            let launch_count = launch_count.clone();
            let completion_count = completion_count.clone();

            async move {
                launch_count.fetch_add(1, Ordering::Relaxed);
                let _ = receiver1.await;
                completion_count.fetch_add(1, Ordering::Relaxed);
            }
        });

        executor.run();

        // Check that the executor returns on deadlock, i.e. none of the task has
        // completed.
        assert_eq!(launch_count.load(Ordering::Relaxed), 2);
        assert_eq!(completion_count.load(Ordering::Relaxed), 0);

        // Drop the executor and thus the receiver tasks before the senders,
        // failing which the senders may signal that the channel has been
        // dropped and wake the tasks outside the executor.
        drop(executor);
    }

    fn executor_drop_cycle(mut executor: Executor) {
        let (sender1, mut receiver1) = mpsc::channel(2);
        let (sender2, mut receiver2) = mpsc::channel(2);
        let (sender3, mut receiver3) = mpsc::channel(2);

        let drop_count = Arc::new(AtomicUsize::new(0));

        // Spawn 3 tasks that wake one another when dropped.
        executor.spawn_and_forget({
            let mut sender2 = sender2.clone();
            let mut sender3 = sender3.clone();
            let drop_count = drop_count.clone();

            async move {
                let _guard = RunOnDrop::new(move || {
                    let _ = sender2.try_send(());
                    let _ = sender3.try_send(());
                    drop_count.fetch_add(1, Ordering::Relaxed);
                });
                let _ = receiver1.next().await;
            }
        });
        executor.spawn_and_forget({
            let mut sender1 = sender1.clone();
            let mut sender3 = sender3.clone();
            let drop_count = drop_count.clone();

            async move {
                let _guard = RunOnDrop::new(move || {
                    let _ = sender1.try_send(());
                    let _ = sender3.try_send(());
                    drop_count.fetch_add(1, Ordering::Relaxed);
                });
                let _ = receiver2.next().await;
            }
        });
        executor.spawn_and_forget({
            let mut sender1 = sender1.clone();
            let mut sender2 = sender2.clone();
            let drop_count = drop_count.clone();

            async move {
                let _guard = RunOnDrop::new(move || {
                    let _ = sender1.try_send(());
                    let _ = sender2.try_send(());
                    drop_count.fetch_add(1, Ordering::Relaxed);
                });
                let _ = receiver3.next().await;
            }
        });

        executor.run();

        // Make sure that all tasks are eventually dropped even though each task
        // wakes the others when dropped.
        drop(executor);
        assert_eq!(drop_count.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn executor_deadlock_st() {
        executor_deadlock(Executor::new_single_threaded(dummy_simulation_context()));
    }

    #[test]
    fn executor_deadlock_mt() {
        executor_deadlock(Executor::new_multi_threaded(3, dummy_simulation_context()));
    }

    #[test]
    fn executor_deadlock_mt_one_worker() {
        executor_deadlock(Executor::new_multi_threaded(1, dummy_simulation_context()));
    }
    #[test]
    fn executor_drop_cycle_st() {
        executor_drop_cycle(Executor::new_single_threaded(dummy_simulation_context()));
    }

    #[test]
    fn executor_drop_cycle_mt() {
        executor_drop_cycle(Executor::new_multi_threaded(3, dummy_simulation_context()));
    }
}
