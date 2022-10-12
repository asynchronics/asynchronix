use std::any::Any;
use std::sync::atomic::{self, AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::find_bit;
use super::injector::Injector;
use super::rng;
use super::{GlobalQueue, Stealer};

#[derive(Debug)]
pub(crate) struct Pool {
    pub(crate) global_queue: GlobalQueue,
    pub(crate) executor_id: usize,
    pub(crate) executor_unparker: parking::Unparker,
    state: PoolRegistry,
    stealers: Box<[Stealer]>,
    worker_unparkers: Box<[parking::Unparker]>,
    searching_workers: AtomicUsize,
    terminate_signal: AtomicBool,
    worker_panic: Mutex<Option<Box<dyn Any + Send + 'static>>>,
}

impl Pool {
    /// Creates a new pool.
    pub(crate) fn new(
        executor_id: usize,
        executor_unparker: parking::Unparker,
        shared_data: impl Iterator<Item = (Stealer, parking::Unparker)>,
    ) -> Self {
        let (stealers, worker_unparkers): (Vec<_>, Vec<_>) = shared_data.into_iter().unzip();
        let worker_unparkers = worker_unparkers.into_boxed_slice();

        Self {
            global_queue: Injector::new(),
            executor_id,
            executor_unparker,
            state: PoolRegistry::new(worker_unparkers.len()),
            stealers: stealers.into_boxed_slice(),
            worker_unparkers,
            searching_workers: AtomicUsize::new(0),
            terminate_signal: AtomicBool::new(false),
            worker_panic: Mutex::new(None),
        }
    }

    /// Marks all pool workers as active.
    ///
    /// Unparking the worker threads is the responsibility of the caller.
    pub(crate) fn set_all_workers_active(&self) {
        self.state.set_all_active();
    }

    /// Marks the specified worker as active.
    ///
    /// Unparking the worker thread is the responsibility of the caller.
    pub(crate) fn set_worker_active(&self, worker_id: usize) {
        self.state.set_active(worker_id);
    }

    /// Marks the specified worker as idle.
    ///
    /// Parking the worker thread is the responsibility of the caller.
    ///
    /// If this was the last active worker, the main executor thread is
    /// unparked.
    pub(crate) fn set_worker_inactive(&self, worker_id: usize) -> PoolState {
        self.state.set_inactive(worker_id)
    }

    /// Unparks an idle worker if any is found, or do nothing otherwise.
    ///
    /// For performance reasons, no synchronization is established if no worker
    /// is found, meaning that workers in other threads may later transition to
    /// idle state without observing the tasks scheduled by the caller to this
    /// method. If this is not tolerable (for instance if this method is called
    /// from a non-worker thread), use the more expensive `activate_worker`.
    pub(crate) fn activate_worker_relaxed(&self) {
        if let Some(worker_id) = self.state.set_one_active_relaxed() {
            self.searching_workers.fetch_add(1, Ordering::Relaxed);
            self.worker_unparkers[worker_id].unpark();
        }
    }

    /// Unparks an idle worker if any is found, or ensure that at least the last
    /// worker to transition to idle state will observe all tasks previously
    /// scheduled by the caller to this method.
    pub(crate) fn activate_worker(&self) {
        if let Some(worker_id) = self.state.set_one_active() {
            self.searching_workers.fetch_add(1, Ordering::Relaxed);
            self.worker_unparkers[worker_id].unpark();
        }
    }

    /// Check if the pool is idle, i.e. if no worker is currently active.
    ///
    /// If `true` is returned, it is guaranteed that all operations performed by
    /// the now-inactive workers become visible in this thread.
    pub(crate) fn is_idle(&self) -> bool {
        self.state.pool_state() == PoolState::Idle
    }

    /// Increments the count of workers actively searching for tasks.
    pub(crate) fn begin_worker_search(&self) {
        self.searching_workers.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrements the count of workers actively searching for tasks.
    pub(crate) fn end_worker_search(&self) {
        self.searching_workers.fetch_sub(1, Ordering::Relaxed);
    }

    /// Returns the count of workers actively searching for tasks.
    pub(crate) fn searching_worker_count(&self) -> usize {
        self.searching_workers.load(Ordering::Relaxed)
    }

    /// Triggers the termination signal and unparks all worker threads so they
    /// can cleanly terminate.
    pub(crate) fn trigger_termination(&self) {
        self.terminate_signal.store(true, Ordering::Relaxed);

        self.state.set_all_active();
        for unparker in &*self.worker_unparkers {
            unparker.unpark();
        }
    }

    /// Returns true if the termination signal was triggered.
    pub(crate) fn termination_is_triggered(&self) -> bool {
        self.terminate_signal.load(Ordering::Relaxed)
    }

    /// Registers a panic associated with the provided worker ID.
    ///
    /// If no panic is currently registered, the panic in argument is
    /// registered. If a panic was already registered by a worker and was not
    /// yet processed by the executor, then nothing is done.
    pub(crate) fn register_panic(&self, panic: Box<dyn Any + Send + 'static>) {
        let mut worker_panic = self.worker_panic.lock().unwrap();
        if worker_panic.is_none() {
            *worker_panic = Some(panic);
        }
    }

    /// Takes a worker panic if any is registered.
    pub(crate) fn take_panic(&self) -> Option<Box<dyn Any + Send + 'static>> {
        let mut worker_panic = self.worker_panic.lock().unwrap();
        worker_panic.take()
    }

    /// Returns an iterator yielding the stealers associated with all active
    /// workers, starting from a randomly selected active worker. The worker
    /// which ID is provided in argument (if any) is excluded from the pool of
    /// candidates.
    pub(crate) fn shuffled_stealers<'a>(
        &'a self,
        excluded_worker_id: Option<usize>,
        rng: &'_ rng::Rng,
    ) -> ShuffledStealers<'a> {
        // All active workers except the specified one are candidate for stealing.
        let mut candidates = self.state.get_active();
        if let Some(excluded_worker_id) = excluded_worker_id {
            candidates &= !(1 << excluded_worker_id);
        }

        ShuffledStealers::new(candidates, &self.stealers, rng)
    }
}

pub(crate) struct ShuffledStealers<'a> {
    stealers: &'a [Stealer],
    // A bit-rotated bit field of the remaining candidate workers to steal from.
    // If set, the LSB represents the next candidate.
    candidates: usize,
    next_candidate: usize, // index of the next candidate
}
impl<'a> ShuffledStealers<'a> {
    fn new(candidates: usize, stealers: &'a [Stealer], rng: &'_ rng::Rng) -> Self {
        let (candidates, next_candidate) = if candidates == 0 {
            (0, 0)
        } else {
            let next_candidate = find_bit::find_bit(candidates, |count| {
                rng.gen_bounded(count as u64) as usize + 1
            });

            // Right-rotate the candidates so that the bit corresponding to the
            // randomly selected worker becomes the LSB.
            let candidate_count = stealers.len();
            let lower_mask = (1 << next_candidate) - 1;
            let lower_bits = candidates & lower_mask;
            let candidates =
                (candidates >> next_candidate) | (lower_bits << (candidate_count - next_candidate));

            (candidates, next_candidate)
        };

        Self {
            stealers,
            candidates,
            next_candidate,
        }
    }
}

impl<'a> Iterator for ShuffledStealers<'a> {
    type Item = &'a Stealer;

    fn next(&mut self) -> Option<Self::Item> {
        if self.candidates == 0 {
            return None;
        }

        // Clear the bit corresponding to the current candidate worker.
        self.candidates &= !1;

        let current_candidate = self.next_candidate;

        if self.candidates != 0 {
            // Locate the next candidate worker and make it the LSB.
            let shift = self.candidates.trailing_zeros();
            self.candidates >>= shift;

            // Update the next candidate.
            self.next_candidate += shift as usize;
            if self.next_candidate >= self.stealers.len() {
                self.next_candidate -= self.stealers.len();
            }
        }

        Some(&self.stealers[current_candidate])
    }
}

/// Registry of active/idle worker threads.
///
/// The registry only supports up to `usize::BITS` threads.
#[derive(Debug)]
struct PoolRegistry {
    active_workers: AtomicUsize,
    pool_size: usize,
    #[cfg(feature = "dev-logs")]
    record: Record,
}
impl PoolRegistry {
    /// Creates a new pool registry.
    ///
    /// #Panic
    ///
    /// This will panic if the specified pool size is zero or is more than
    /// `usize::BITS`.
    fn new(pool_size: usize) -> Self {
        assert!(
            pool_size >= 1,
            "the executor pool size should be at least one"
        );
        assert!(
            pool_size <= usize::BITS as usize,
            "the executor pool size should be at most {}",
            usize::BITS
        );

        Self {
            active_workers: AtomicUsize::new(0),
            pool_size,
            #[cfg(feature = "dev-logs")]
            record: Record::new(pool_size),
        }
    }
    /// Returns the state of the pool.
    ///
    /// This operation has Acquire semantic, which guarantees that if the pool
    /// state returned is `PoolState::Idle`, then all operations performed by
    /// the now-inactive workers are visible.
    fn pool_state(&self) -> PoolState {
        // Ordering: this Acquire operation synchronizes with all Release
        // RMWs in the `set_inactive` method via a release sequence.
        let active_workers = self.active_workers.load(Ordering::Acquire);
        if active_workers == 0 {
            PoolState::Idle
        } else {
            PoolState::Busy
        }
    }

    /// Marks the specified worker as inactive.
    ///
    /// The specified worker must currently be marked as active. Returns
    /// `PoolState::Idle` if this was the last active thread.
    ///
    /// If this is the last active worker (i.e. `PoolState::Idle` is returned),
    /// then it is guaranteed that all operations performed by the now-inactive
    /// workers and by unsuccessful callers to `set_one_active` are now visible.
    fn set_inactive(&self, worker_id: usize) -> PoolState {
        // Ordering: this Release operation synchronizes with the Acquire
        // fence in the below conditional when the pool becomes idle, and/or
        // with the Acquire state load in the `pool_state` method.
        let active_workers = self
            .active_workers
            .fetch_and(!(1 << worker_id), Ordering::Release);

        if active_workers & !(1 << worker_id) == 0 {
            // Ordering: this Acquire fence synchronizes with all Release
            // RMWs in this and in the previous calls to `set_inactive` via a
            // release sequence.
            atomic::fence(Ordering::Acquire);
            PoolState::Idle
        } else {
            PoolState::Busy
        }
    }

    /// Marks the specified worker as active.
    fn set_active(&self, worker_id: usize) {
        self.active_workers
            .fetch_or(1 << worker_id, Ordering::Relaxed);
    }

    /// Marks all workers as active.
    fn set_all_active(&self) {
        // Mark all workers as busy.
        self.active_workers.store(
            !0 >> (usize::BITS - self.pool_size as u32),
            Ordering::Relaxed,
        );
    }

    /// Marks a worker as active if any is found, otherwise do nothing.
    ///
    /// The worker ID is returned if successful.
    fn set_one_active_relaxed(&self) -> Option<usize> {
        let mut active_workers = self.active_workers.load(Ordering::Relaxed);
        loop {
            let first_idle_worker = active_workers.trailing_ones() as usize;
            if first_idle_worker >= self.pool_size {
                return None;
            };
            active_workers = self
                .active_workers
                .fetch_or(1 << first_idle_worker, Ordering::Relaxed);
            if active_workers & (1 << first_idle_worker) == 0 {
                #[cfg(feature = "dev-logs")]
                self.record.increment(first_idle_worker);
                return Some(first_idle_worker);
            }
        }
    }

    /// Marks a worker as active if any is found, otherwise ensure that all
    /// memory operations made by the caller prior to this call are visible by
    /// the last worker transitioning to idle state.
    ///
    /// The worker ID is returned if successful.
    fn set_one_active(&self) -> Option<usize> {
        let mut active_workers = self.active_workers.load(Ordering::Relaxed);
        loop {
            let first_idle_worker = active_workers.trailing_ones() as usize;

            if first_idle_worker >= self.pool_size {
                // There is apparently no free worker, so a dummy RMW with
                // Release ordering is performed with the sole purpose of
                // synchronizing with the Acquire fence in `set_inactive` so
                // that the last worker to transition to idle can see the tasks
                // that were queued prior to this call.
                let new_active_workers = self.active_workers.fetch_or(0, Ordering::Release);
                if new_active_workers == active_workers {
                    return None;
                }
                active_workers = new_active_workers;
            } else {
                active_workers = self
                    .active_workers
                    .fetch_or(1 << first_idle_worker, Ordering::Relaxed);
                if active_workers & (1 << first_idle_worker) == 0 {
                    #[cfg(feature = "dev-logs")]
                    self.record.increment(first_idle_worker);
                    return Some(first_idle_worker);
                }
            }
        }
    }

    /// Returns a bit field that indicates all active workers.
    fn get_active(&self) -> usize {
        self.active_workers.load(Ordering::Relaxed)
    }
}

#[derive(PartialEq)]
pub(crate) enum PoolState {
    Idle,
    Busy,
}

#[cfg(feature = "dev-logs")]
impl Drop for PoolRegistry {
    fn drop(&mut self) {
        println!("Thread launch count: {:?}", self.record.get());
    }
}

#[cfg(feature = "dev-logs")]
#[derive(Debug)]
struct Record {
    stats: Vec<AtomicUsize>,
}

#[cfg(feature = "dev-logs")]
impl Record {
    fn new(worker_count: usize) -> Self {
        let mut stats = Vec::new();
        stats.resize_with(worker_count, Default::default);
        Self { stats }
    }
    fn increment(&self, worker_id: usize) {
        self.stats[worker_id].fetch_add(1, Ordering::Relaxed);
    }
    fn get(&self) -> Vec<usize> {
        self.stats
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .collect()
    }
}
