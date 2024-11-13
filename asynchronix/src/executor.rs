//! `async` executor trait.

mod mt_executor;
mod st_executor;
mod task;

use std::any::Any;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_utils::CachePadded;

use crate::macros::scoped_thread_local::scoped_thread_local;
use crate::simulation::ModelId;
#[cfg(feature = "tracing")]
use crate::time::AtomicTimeReader;
use task::Promise;

/// Unique identifier for executor instances.
static NEXT_EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) enum ExecutorError {
    /// The simulation has deadlocked.
    Deadlock,
    /// The simulation has timed out.
    Timeout,
    /// The simulation has panicked.
    Panic(ModelId, Box<dyn Any + Send + 'static>),
}

/// Context common to all executor types.
#[derive(Clone)]
pub(crate) struct SimulationContext {
    /// Read-only handle to the simulation time.
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
    pub(crate) fn new_single_threaded(
        simulation_context: SimulationContext,
        abort_signal: Signal,
    ) -> Self {
        Self::StExecutor(st_executor::Executor::new(simulation_context, abort_signal))
    }

    /// Creates an executor that runs futures on a thread pool.
    ///
    /// The maximum number of threads is set with the `num_threads` parameter.
    ///
    /// # Panics
    ///
    /// This will panic if the specified number of threads is zero or more than
    /// `usize::BITS`.
    pub(crate) fn new_multi_threaded(
        num_threads: usize,
        simulation_context: SimulationContext,
        abort_signal: Signal,
    ) -> Self {
        Self::MtExecutor(mt_executor::Executor::new(
            num_threads,
            simulation_context,
            abort_signal,
        ))
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
    pub(crate) fn run(&mut self, timeout: Duration) -> Result<(), ExecutorError> {
        match self {
            Self::StExecutor(executor) => executor.run(timeout),
            Self::MtExecutor(executor) => executor.run(timeout),
        }
    }
}

/// A single-use shared boolean signal.
#[derive(Clone, Debug)]
pub(crate) struct Signal(Arc<CachePadded<AtomicBool>>);

impl Signal {
    /// Create a new, cleared signal.
    pub(crate) fn new() -> Self {
        Self(Arc::new(CachePadded::new(AtomicBool::new(false))))
    }

    /// Sets the signal.
    pub(crate) fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns `true``is the signal was set.
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use futures_channel::mpsc;
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

        executor.run(Duration::ZERO).unwrap();

        // Make sure that all tasks are eventually dropped even though each task
        // wakes the others when dropped.
        drop(executor);
        assert_eq!(drop_count.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn executor_drop_cycle_st() {
        executor_drop_cycle(Executor::new_single_threaded(
            dummy_simulation_context(),
            Signal::new(),
        ));
    }

    #[test]
    fn executor_drop_cycle_mt() {
        executor_drop_cycle(Executor::new_multi_threaded(
            3,
            dummy_simulation_context(),
            Signal::new(),
        ));
    }
}
