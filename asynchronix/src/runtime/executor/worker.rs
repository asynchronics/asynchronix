use std::cell::Cell;
use std::sync::Arc;

use super::task::Runnable;

use super::ExecutorContext;
use super::LocalQueue;

/// A local worker with access to global executor resources.
pub(crate) struct Worker {
    pub(super) local_queue: LocalQueue,
    pub(super) fast_slot: Cell<Option<Runnable>>,
    pub(super) executor_context: Arc<ExecutorContext>,
}

impl Worker {
    /// Creates a new worker.
    pub(super) fn new(local_queue: LocalQueue, executor_context: Arc<ExecutorContext>) -> Self {
        Self {
            local_queue,
            fast_slot: Cell::new(None),
            executor_context,
        }
    }
}
