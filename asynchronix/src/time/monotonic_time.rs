//! Monotonic simulation time.
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

use super::MonotonicTime;

use crate::util::sync_cell::TearableAtomic;

/// A tearable atomic adapter over a `MonotonicTime`.
///
/// This makes it possible to store the simulation time in a `SyncCell`, an
/// efficient, seqlock-based alternative to `RwLock`.
pub(crate) struct TearableAtomicTime {
    secs: AtomicI64,
    nanos: AtomicU32,
}

impl TearableAtomicTime {
    pub(crate) fn new(time: MonotonicTime) -> Self {
        Self {
            secs: AtomicI64::new(time.as_secs()),
            nanos: AtomicU32::new(time.subsec_nanos()),
        }
    }
}

impl TearableAtomic for TearableAtomicTime {
    type Value = MonotonicTime;

    fn tearable_load(&self) -> MonotonicTime {
        // Load each field separately. This can never create invalid values of a
        // `MonotonicTime`, even if the load is torn.
        MonotonicTime::new(
            self.secs.load(Ordering::Relaxed),
            self.nanos.load(Ordering::Relaxed),
        )
        .unwrap()
    }

    fn tearable_store(&self, value: MonotonicTime) {
        // Write each field separately. This can never create invalid values of
        // a `MonotonicTime`, even if the store is torn.
        self.secs.store(value.as_secs(), Ordering::Relaxed);
        self.nanos.store(value.subsec_nanos(), Ordering::Relaxed);
    }
}
