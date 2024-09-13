//! Unstable, unofficial public API meant for external benchmarking and testing.
//!
//! Not for production use!

use std::future::Future;

use crate::executor;

/// A multi-threaded `async` executor.
#[derive(Debug)]
pub struct Executor(executor::Executor);

impl Executor {
    /// Creates an executor that runs futures on a thread pool.
    ///
    /// The maximum number of threads is set with the `pool_size` parameter.
    pub fn new(pool_size: usize) -> Self {
        let dummy_context = crate::executor::SimulationContext {
            #[cfg(feature = "tracing")]
            time_reader: crate::util::sync_cell::SyncCell::new(
                crate::time::TearableAtomicTime::new(crate::time::MonotonicTime::EPOCH),
            )
            .reader(),
        };
        Self(executor::Executor::new_multi_threaded(
            pool_size,
            dummy_context,
        ))
    }

    /// Spawns a task which output will never be retrieved.
    ///
    /// This is mostly useful to avoid undue reference counting for futures that
    /// return a `()` type.
    pub fn spawn_and_forget<T>(&self, future: T)
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        self.0.spawn_and_forget(future);
    }

    /// Let the executor run, blocking until all futures have completed or until
    /// the executor deadlocks.
    pub fn run(&mut self) {
        self.0.run();
    }
}
