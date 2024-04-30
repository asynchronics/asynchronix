use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError};

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
    local: T,
    local_epoch: usize,
    shared: Arc<Mutex<T>>,
    epoch: Arc<AtomicUsize>,
}

impl<T: Clone> CachedRwLock<T> {
    /// Creates a new cached read-write lock in an ulocked state.
    pub(crate) fn new(t: T) -> Self {
        let shared = t.clone();
        Self {
            local: t,
            local_epoch: 0,
            shared: Arc::new(Mutex::new(shared)),
            epoch: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Gives access to the local cache without synchronization.
    pub(crate) fn read_unsync(&self) -> &T {
        &self.local
    }

    /// Synchronizes the local cache if it is behind the shared data and gives
    /// access to it.
    #[allow(dead_code)]
    pub(crate) fn read(&mut self) -> LockResult<&T> {
        if self.epoch.load(Ordering::Relaxed) != self.local_epoch {
            match self.shared.lock() {
                LockResult::Ok(shared) => {
                    self.local = shared.clone();
                    self.local_epoch = self.epoch.load(Ordering::Relaxed)
                }
                LockResult::Err(_) => return LockResult::Err(PoisonError::new(&self.local)),
            }
        }
        LockResult::Ok(&self.local)
    }

    /// Gives write access to the local cache without synchronization so it can
    /// be used as a scratchpad.
    #[allow(dead_code)]
    pub(crate) fn write_scratchpad_unsync(&mut self) -> &mut T {
        &mut self.local
    }

    /// Synchronizes the local cache if it is behind the shared data and gives
    /// write access to it so it can be used as a scratchpad.
    pub(crate) fn write_scratchpad(&mut self) -> LockResult<&mut T> {
        if self.epoch.load(Ordering::Relaxed) != self.local_epoch {
            match self.shared.lock() {
                LockResult::Ok(shared) => {
                    self.local = shared.clone();
                    self.local_epoch = self.epoch.load(Ordering::Relaxed)
                }
                LockResult::Err(_) => return LockResult::Err(PoisonError::new(&mut self.local)),
            }
        }
        LockResult::Ok(&mut self.local)
    }

    /// Acquires a write lock on the shared data.
    pub(crate) fn write(&mut self) -> LockResult<CachedRwLockWriteGuard<'_, T>> {
        let guard = self.shared.lock();
        let epoch = self.epoch.load(Ordering::Relaxed) + 1;
        self.epoch.store(epoch, Ordering::Relaxed);

        match guard {
            LockResult::Ok(shared) => LockResult::Ok(CachedRwLockWriteGuard { guard: shared }),
            LockResult::Err(poison) => LockResult::Err(PoisonError::new(CachedRwLockWriteGuard {
                guard: poison.into_inner(),
            })),
        }
    }
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
