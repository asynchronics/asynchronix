use std::fmt;
use std::sync::{Arc, Mutex};

use crate::executor::Executor;
use crate::model::Model;
use crate::time::{Clock, NoClock};
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::priority_queue::PriorityQueue;
use crate::util::sync_cell::SyncCell;

use super::{add_model, Mailbox, SchedulerQueue, Simulation};

/// Builder for a multi-threaded, discrete-event simulation.
pub struct SimInit {
    executor: Executor,
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: SyncCell<TearableAtomicTime>,
    clock: Box<dyn Clock + 'static>,
}

impl SimInit {
    /// Creates a builder for a multithreaded simulation running on all
    /// available logical threads.
    pub fn new() -> Self {
        Self::with_num_threads(num_cpus::get())
    }

    /// Creates a builder for a multithreaded simulation running on the
    /// specified number of threads.
    pub fn with_num_threads(num_threads: usize) -> Self {
        // The current executor's implementation caps the number of thread to 64
        // on 64-bit systems and 32 on 32-bit systems.
        let num_threads = num_threads.min(usize::BITS as usize);

        Self {
            executor: Executor::new(num_threads),
            scheduler_queue: Arc::new(Mutex::new(PriorityQueue::new())),
            time: SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)),
            clock: Box::new(NoClock::new()),
        }
    }

    /// Adds a model and its mailbox to the simulation bench.
    ///
    /// The `name` argument needs not be unique (it can be the empty string) and
    /// is used for convenience for the model instance identification (e.g. for
    /// logging purposes).
    pub fn add_model<M: Model>(
        self,
        model: M,
        mailbox: Mailbox<M>,
        name: impl Into<String>,
    ) -> Self {
        let scheduler_queue = self.scheduler_queue.clone();
        let time = self.time.reader();

        add_model(
            model,
            mailbox,
            name.into(),
            scheduler_queue,
            time,
            &self.executor,
        );

        self
    }

    /// Synchronize the simulation with the provided [`Clock`].
    ///
    /// If the clock isn't explicitly set then the default [`NoClock`] is used,
    /// resulting in the simulation running as fast as possible.
    pub fn set_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Box::new(clock);

        self
    }

    /// Builds a simulation initialized at the specified simulation time,
    /// executing the [`Model::init()`](crate::model::Model::init) method on all
    /// model initializers.
    pub fn init(mut self, start_time: MonotonicTime) -> Simulation {
        self.time.write(start_time);
        self.clock.synchronize(start_time);
        self.executor.run();

        Simulation::new(self.executor, self.scheduler_queue, self.time, self.clock)
    }
}

impl Default for SimInit {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SimInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimInit").finish_non_exhaustive()
    }
}
