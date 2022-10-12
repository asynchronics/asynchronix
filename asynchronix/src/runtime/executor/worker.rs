use std::cell::Cell;
use std::sync::Arc;

use super::task::Runnable;

use super::pool::Pool;
use super::LocalQueue;

/// A local worker with access to global executor resources.
pub(crate) struct Worker {
    pub(crate) local_queue: LocalQueue,
    pub(crate) fast_slot: Cell<Option<Runnable>>,
    pub(crate) pool: Arc<Pool>,
}

impl Worker {
    /// Creates a new worker.
    pub(crate) fn new(local_queue: LocalQueue, pool: Arc<Pool>) -> Self {
        Self {
            local_queue,
            fast_slot: Cell::new(None),
            pool,
        }
    }
}
