use super::{CLOSED, POLLING, WAKE_MASK};

/// An object that runs an arbitrary closure when dropped.
pub(crate) struct RunOnDrop<F: FnMut()> {
    drop_fn: F,
}
impl<F: FnMut()> RunOnDrop<F> {
    /// Creates a new `RunOnDrop`.
    pub(crate) fn new(drop_fn: F) -> Self {
        Self { drop_fn }
    }
}
impl<F: FnMut()> Drop for RunOnDrop<F> {
    fn drop(&mut self) {
        (self.drop_fn)();
    }
}

/// Check if a `Runnable` exists based on the state.
#[inline(always)]
pub(crate) fn runnable_exists(state: u64) -> bool {
    state & POLLING != 0 && state & (WAKE_MASK | CLOSED) != 0
}
