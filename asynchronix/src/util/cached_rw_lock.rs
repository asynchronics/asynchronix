//! Cached read-write lock.

use std::ops::{Deref, DerefMut};

use crate::loom_exports::sync::atomic::{AtomicUsize, Ordering};
use crate::loom_exports::sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError};

/// A cached read-write lock.
///
/// This read-write lock maintains a local cache in each clone for read
/// access. Regular writes are always synchronized and performed on the shared
/// data. Regular reads are synchronized only when the shared data has been
/// modified since the local cache was last synchronized. The local cache can
/// alternatively be used as a scratchpad without invalidating the shared data,
/// in which case all changes to the scratchpad will be lost on the next
/// synchronization.
#[derive(Clone)]
pub(crate) struct CachedRwLock<T: Clone> {
    value: T,
    epoch: usize,
    shared: Arc<Shared<T>>,
}

impl<T: Clone> CachedRwLock<T> {
    /// Creates a new cached read-write lock in unlocked state.
    pub(crate) fn new(t: T) -> Self {
        let shared = t.clone();
        Self {
            value: t,
            epoch: 0,
            shared: Arc::new(Shared {
                value: Mutex::new(shared),
                epoch: AtomicUsize::new(0),
            }),
        }
    }

    /// Gives access to the local cache without synchronization.
    pub(crate) fn read_unsync(&self) -> &T {
        &self.value
    }

    /// Synchronizes the local cache if it is behind the shared data and gives
    /// access to it.
    #[allow(dead_code)]
    pub(crate) fn read(&mut self) -> LockResult<&T> {
        if self.shared.epoch.load(Ordering::Relaxed) != self.epoch {
            match self.shared.value.lock() {
                LockResult::Ok(shared) => {
                    self.value = shared.clone();
                    self.epoch = self.shared.epoch.load(Ordering::Relaxed)
                }
                LockResult::Err(_) => return LockResult::Err(PoisonError::new(&self.value)),
            }
        }
        LockResult::Ok(&self.value)
    }

    /// Gives write access to the local cache without synchronization so it can
    /// be used as a scratchpad.
    #[allow(dead_code)]
    pub(crate) fn write_scratchpad_unsync(&mut self) -> &mut T {
        &mut self.value
    }

    /// Synchronizes the local cache if it is behind the shared data and gives
    /// write access to it so it can be used as a scratchpad.
    pub(crate) fn write_scratchpad(&mut self) -> LockResult<&mut T> {
        if self.shared.epoch.load(Ordering::Relaxed) != self.epoch {
            match self.shared.value.lock() {
                LockResult::Ok(shared) => {
                    self.value = shared.clone();
                    self.epoch = self.shared.epoch.load(Ordering::Relaxed)
                }
                LockResult::Err(_) => return LockResult::Err(PoisonError::new(&mut self.value)),
            }
        }
        LockResult::Ok(&mut self.value)
    }

    /// Acquires a write lock on the shared data.
    pub(crate) fn write(&mut self) -> LockResult<CachedRwLockWriteGuard<'_, T>> {
        let guard = self.shared.value.lock();
        let epoch = self.shared.epoch.load(Ordering::Relaxed) + 1;
        self.shared.epoch.store(epoch, Ordering::Relaxed);

        match guard {
            LockResult::Ok(shared) => LockResult::Ok(CachedRwLockWriteGuard { guard: shared }),
            LockResult::Err(poison) => LockResult::Err(PoisonError::new(CachedRwLockWriteGuard {
                guard: poison.into_inner(),
            })),
        }
    }
}

struct Shared<T> {
    epoch: AtomicUsize,
    value: Mutex<T>,
}

/// Write guard.
///
/// The lock is released when the guard is dropped.
pub(crate) struct CachedRwLockWriteGuard<'a, T: Clone> {
    guard: MutexGuard<'a, T>,
}

impl<T: Clone> Deref for CachedRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: Clone> DerefMut for CachedRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(all(test, asynchronix_loom))]
mod tests {
    use super::*;

    use loom::model::Builder;
    use loom::thread;

    #[test]
    fn loom_cached_rw_lock_write() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        const ITERATIONS_NUMBER: usize = 5;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let mut writer0: CachedRwLock<usize> = CachedRwLock::new(0);
            let mut writer1 = writer0.clone();
            let mut reader = writer0.clone();

            let th_w = thread::spawn(move || {
                for _ in 0..ITERATIONS_NUMBER {
                    let mut guard = writer0.write().unwrap();
                    *guard = *guard + 1;
                }
            });

            let th_r = thread::spawn(move || {
                let mut value = 0;
                let mut prev_value;
                for _ in 0..ITERATIONS_NUMBER {
                    prev_value = value;
                    value = *reader.write_scratchpad().unwrap();
                    assert!(
                        prev_value <= value,
                        "Previous value = {}, value = {}",
                        prev_value,
                        value
                    );
                    assert_eq!(value, reader.epoch);
                }
            });

            for _ in 0..ITERATIONS_NUMBER {
                let mut guard = writer1.write().unwrap();
                *guard = *guard + 1;
            }

            th_w.join().unwrap();
            th_r.join().unwrap();
        });
    }
}
