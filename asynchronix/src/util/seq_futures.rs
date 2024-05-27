//! Sequential composition of futures into a single future.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// An owned future which sequentially polls a collection of futures.
///
/// The outputs of the futures, if any, are ignored. For simplicity, the
/// implementation assumes that the futures are `Unpin`. This constrained could
/// be relaxed if necessary by using something else than a `Vec` to ensure that
/// each future is pinned (a `Vec` is not suitable for pinning because it may
/// move its items when dropped).
pub(crate) struct SeqFuture<F> {
    inner: Vec<F>,
    idx: usize,
}

impl<F> SeqFuture<F> {
    /// Creates a new, empty `SeqFuture`.
    pub(crate) fn new() -> Self {
        Self {
            inner: Vec::new(),
            idx: 0,
        }
    }

    /// Appends a future.
    pub(crate) fn push(&mut self, future: F) {
        self.inner.push(future);
    }
}

impl<F: Future + Unpin> Future for SeqFuture<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        // The below will panic due to out of bound access when polling after
        // completion: this is intentional.
        while Pin::new(&mut this.inner[this.idx]).poll(cx).is_ready() {
            this.idx += 1;
            if this.idx == this.inner.len() {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}
