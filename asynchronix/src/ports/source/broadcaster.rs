use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::vec;

use pin_project_lite::pin_project;

use diatomic_waker::WakeSink;

use super::sender::{Sender, SenderFuture};

use crate::ports::LineId;
use crate::util::task_set::TaskSet;

/// An object that can efficiently broadcast messages to several addresses.
///
/// This is very similar to `output::broadcaster::BroadcasterInner`, but
/// generates owned futures instead.
///
/// This object maintains a list of senders associated to each target address.
/// When a message is broadcast, the sender futures are awaited in parallel.
/// This is somewhat similar to what `FuturesOrdered` in the `futures` crate
/// does, but the outputs of all sender futures are returned all at once rather
/// than with an asynchronous iterator (a.k.a. async stream).
pub(super) struct BroadcasterInner<T: Clone, R> {
    /// Line identifier for the next port to be connected.
    next_line_id: u64,
    /// The list of senders with their associated line identifier.
    senders: Vec<(LineId, Box<dyn Sender<T, R>>)>,
}

impl<T: Clone, R> BroadcasterInner<T, R> {
    /// Adds a new sender associated to the specified identifier.
    ///
    /// # Panics
    ///
    /// This method will panic if the total count of senders would reach
    /// `u32::MAX - 1`.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, R>>) -> LineId {
        assert!(self.next_line_id != u64::MAX);
        let line_id = LineId(self.next_line_id);
        self.next_line_id += 1;

        self.senders.push((line_id, sender));

        line_id
    }

    /// Removes the first sender with the specified identifier, if any.
    ///
    /// Returns `true` if there was indeed a sender associated to the specified
    /// identifier.
    pub(super) fn remove(&mut self, id: LineId) -> bool {
        if let Some(pos) = self.senders.iter().position(|s| s.0 == id) {
            self.senders.swap_remove(pos);

            return true;
        }

        false
    }

    /// Removes all senders.
    pub(super) fn clear(&mut self) {
        self.senders.clear();
    }

    /// Returns the number of connected senders.
    pub(super) fn len(&self) -> usize {
        self.senders.len()
    }

    /// Efficiently broadcasts a message or a query to multiple addresses.
    ///
    /// This method does not collect the responses from queries.
    fn broadcast(&mut self, arg: T) -> BroadcastFuture<R> {
        let mut future_states = Vec::with_capacity(self.senders.len());

        // Broadcast the message and collect all futures.
        let mut iter = self.senders.iter_mut();
        while let Some(sender) = iter.next() {
            // Move the argument rather than clone it for the last future.
            if iter.len() == 0 {
                future_states.push(SenderFutureState::Pending(sender.1.send(arg)));
                break;
            }

            future_states.push(SenderFutureState::Pending(sender.1.send(arg.clone())));
        }

        // Generate the global future.
        BroadcastFuture::new(future_states)
    }
}

impl<T: Clone, R> Default for BroadcasterInner<T, R> {
    fn default() -> Self {
        Self {
            next_line_id: 0,
            senders: Vec::new(),
        }
    }
}

/// An object that can efficiently broadcast events to several input ports.
///
/// This is very similar to `output::broadcaster::EventBroadcaster`, but
/// generates owned futures instead.
///
/// See `BroadcasterInner` for implementation details.
pub(super) struct EventBroadcaster<T: Clone> {
    /// The broadcaster core object.
    inner: BroadcasterInner<T, ()>,
}

impl<T: Clone + Send> EventBroadcaster<T> {
    /// Adds a new sender associated to the specified identifier.
    ///
    /// # Panics
    ///
    /// This method will panic if the total count of senders would reach
    /// `u32::MAX - 1`.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, ()>>) -> LineId {
        self.inner.add(sender)
    }

    /// Removes the first sender with the specified identifier, if any.
    ///
    /// Returns `true` if there was indeed a sender associated to the specified
    /// identifier.
    pub(super) fn remove(&mut self, id: LineId) -> bool {
        self.inner.remove(id)
    }

    /// Removes all senders.
    pub(super) fn clear(&mut self) {
        self.inner.clear();
    }

    /// Returns the number of connected senders.
    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Broadcasts an event to all addresses.
    pub(super) fn broadcast(
        &mut self,
        arg: T,
    ) -> impl Future<Output = Result<(), BroadcastError>> + Send {
        enum Fut<F1, F2> {
            Empty,
            Single(F1),
            Multiple(F2),
        }

        let fut = match self.inner.senders.as_mut_slice() {
            // No sender.
            [] => Fut::Empty,
            // One sender.
            [sender] => Fut::Single(sender.1.send(arg)),
            // Multiple senders.
            _ => Fut::Multiple(self.inner.broadcast(arg)),
        };

        async {
            match fut {
                Fut::Empty => Ok(()),
                Fut::Single(fut) => fut.await.map_err(|_| BroadcastError {}),
                Fut::Multiple(fut) => fut.await.map(|_| ()),
            }
        }
    }
}

impl<T: Clone> Default for EventBroadcaster<T> {
    fn default() -> Self {
        Self {
            inner: BroadcasterInner::default(),
        }
    }
}

/// An object that can efficiently broadcast queries to several replier ports.
///
/// This is very similar to `output::broadcaster::QueryBroadcaster`, but
/// generates owned futures instead.
///
/// See `BroadcasterInner` for implementation details.
pub(super) struct QueryBroadcaster<T: Clone, R> {
    /// The broadcaster core object.
    inner: BroadcasterInner<T, R>,
}

impl<T: Clone + Send, R: Send> QueryBroadcaster<T, R> {
    /// Adds a new sender associated to the specified identifier.
    ///
    /// # Panics
    ///
    /// This method will panic if the total count of senders would reach
    /// `u32::MAX - 1`.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, R>>) -> LineId {
        self.inner.add(sender)
    }

    /// Removes the first sender with the specified identifier, if any.
    ///
    /// Returns `true` if there was indeed a sender associated to the specified
    /// identifier.
    pub(super) fn remove(&mut self, id: LineId) -> bool {
        self.inner.remove(id)
    }

    /// Removes all senders.
    pub(super) fn clear(&mut self) {
        self.inner.clear();
    }

    /// Returns the number of connected senders.
    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Broadcasts an event to all addresses.
    pub(super) fn broadcast(
        &mut self,
        arg: T,
    ) -> impl Future<Output = Result<ReplyIterator<R>, BroadcastError>> + Send {
        enum Fut<F1, F2> {
            Empty,
            Single(F1),
            Multiple(F2),
        }

        let fut = match self.inner.senders.as_mut_slice() {
            // No sender.
            [] => Fut::Empty,
            // One sender.
            [sender] => Fut::Single(sender.1.send(arg)),
            // Multiple senders.
            _ => Fut::Multiple(self.inner.broadcast(arg)),
        };

        async {
            match fut {
                Fut::Empty => Ok(ReplyIterator(Vec::new().into_iter())),
                Fut::Single(fut) => fut
                    .await
                    .map(|reply| ReplyIterator(vec![SenderFutureState::Ready(reply)].into_iter()))
                    .map_err(|_| BroadcastError {}),
                Fut::Multiple(fut) => fut.await.map_err(|_| BroadcastError {}),
            }
        }
    }
}

impl<T: Clone, R> Default for QueryBroadcaster<T, R> {
    fn default() -> Self {
        Self {
            inner: BroadcasterInner::default(),
        }
    }
}

pin_project! {
    /// A future aggregating the outputs of a collection of sender futures.
    ///
    /// The idea is to join all sender futures as efficiently as possible, meaning:
    ///
    /// - the sender futures are polled simultaneously rather than waiting for their
    ///   completion in a sequential manner,
    /// - the happy path (all futures immediately ready) is very fast.
    pub(super) struct BroadcastFuture<R> {
        // Thread-safe waker handle.
        wake_sink: WakeSink,
        // Tasks associated to the sender futures.
        task_set: TaskSet,
        // List of all sender futures or their outputs.
        future_states: Vec<SenderFutureState<R>>,
        // The total count of futures that have not yet been polled to completion.
        pending_futures_count: usize,
        // State of completion of the future.
        state: FutureState,
    }
}

impl<R> BroadcastFuture<R> {
    /// Creates a new `BroadcastFuture`.
    fn new(future_states: Vec<SenderFutureState<R>>) -> Self {
        let wake_sink = WakeSink::new();
        let wake_src = wake_sink.source();
        let pending_futures_count = future_states.len();

        BroadcastFuture {
            wake_sink,
            task_set: TaskSet::with_len(wake_src, pending_futures_count),
            future_states,
            pending_futures_count,
            state: FutureState::Uninit,
        }
    }
}

impl<R> Future for BroadcastFuture<R> {
    type Output = Result<ReplyIterator<R>, BroadcastError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        assert_ne!(this.state, FutureState::Completed);

        // Poll all sender futures once if this is the first time the broadcast
        // future is polled.
        if this.state == FutureState::Uninit {
            for task_idx in 0..this.future_states.len() {
                if let SenderFutureState::Pending(future) = &mut this.future_states[task_idx] {
                    let task_waker_ref = this.task_set.waker_of(task_idx);
                    let task_cx_ref = &mut Context::from_waker(&task_waker_ref);

                    match future.as_mut().poll(task_cx_ref) {
                        Poll::Ready(Ok(output)) => {
                            this.future_states[task_idx] = SenderFutureState::Ready(output);
                            this.pending_futures_count -= 1;
                        }
                        Poll::Ready(Err(_)) => {
                            this.state = FutureState::Completed;

                            return Poll::Ready(Err(BroadcastError {}));
                        }
                        Poll::Pending => {}
                    }
                }
            }

            if this.pending_futures_count == 0 {
                this.state = FutureState::Completed;
                let outputs = mem::take(&mut this.future_states).into_iter();

                return Poll::Ready(Ok(ReplyIterator(outputs)));
            }

            this.state = FutureState::Pending;
        }

        // Repeatedly poll the futures of all scheduled tasks until there are no
        // more scheduled tasks.
        loop {
            // No need to register the waker if some tasks have been scheduled.
            if !this.task_set.has_scheduled() {
                this.wake_sink.register(cx.waker());
            }

            // Retrieve the indices of the scheduled tasks if any. If there are
            // no scheduled tasks, `Poll::Pending` is returned and this future
            // will be awaken again when enough tasks have been scheduled.
            //
            // NOTE: the current implementation requires a notification to be
            // sent each time a sub-future has made progress. We may try at some
            // point to benchmark an alternative strategy where a notification
            // is requested only when all pending sub-futures have made progress,
            // using `take_scheduled(this.pending_futures_count)`. This would
            // reduce the cost of context switch but could hurt latency.
            let scheduled_tasks = match this.task_set.take_scheduled(1) {
                Some(st) => st,
                None => return Poll::Pending,
            };

            for task_idx in scheduled_tasks {
                if let SenderFutureState::Pending(future) = &mut this.future_states[task_idx] {
                    let task_waker_ref = this.task_set.waker_of(task_idx);
                    let task_cx_ref = &mut Context::from_waker(&task_waker_ref);

                    match future.as_mut().poll(task_cx_ref) {
                        Poll::Ready(Ok(output)) => {
                            this.future_states[task_idx] = SenderFutureState::Ready(output);
                            this.pending_futures_count -= 1;
                        }
                        Poll::Ready(Err(_)) => {
                            this.state = FutureState::Completed;

                            return Poll::Ready(Err(BroadcastError {}));
                        }
                        Poll::Pending => {}
                    }
                }
            }

            if this.pending_futures_count == 0 {
                this.state = FutureState::Completed;
                let outputs = mem::take(&mut this.future_states).into_iter();

                return Poll::Ready(Ok(ReplyIterator(outputs)));
            }
        }
    }
}

/// Error returned when a message could not be delivered.
#[derive(Debug)]
pub(super) struct BroadcastError {}

#[derive(Debug, PartialEq)]
enum FutureState {
    Uninit,
    Pending,
    Completed,
}

/// The state of a `SenderFuture`.
enum SenderFutureState<R> {
    Pending(SenderFuture<R>),
    Ready(R),
}

/// An iterator over the replies to a broadcasted request.
pub(crate) struct ReplyIterator<R>(vec::IntoIter<SenderFutureState<R>>);

impl<R> Iterator for ReplyIterator<R> {
    type Item = R;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|state| match state {
            SenderFutureState::Ready(reply) => reply,
            _ => panic!("reply missing in replies iterator"),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use futures_executor::block_on;

    use crate::channel::Receiver;
    use crate::model::Context;
    use crate::simulation::{Address, LocalScheduler, Scheduler};
    use crate::time::{MonotonicTime, TearableAtomicTime};
    use crate::util::priority_queue::PriorityQueue;
    use crate::util::sync_cell::SyncCell;

    use super::super::sender::{InputSender, ReplierSender};
    use super::*;
    use crate::model::Model;

    struct Counter {
        inner: Arc<AtomicUsize>,
    }
    impl Counter {
        fn new(counter: Arc<AtomicUsize>) -> Self {
            Self { inner: counter }
        }
        async fn inc(&mut self, by: usize) {
            self.inner.fetch_add(by, Ordering::Relaxed);
        }
        async fn fetch_inc(&mut self, by: usize) -> usize {
            let res = self.inner.fetch_add(by, Ordering::Relaxed);
            res
        }
    }
    impl Model for Counter {}

    #[test]
    fn broadcast_event_smoke() {
        const N_RECV: usize = 4;

        let mut mailboxes = Vec::new();
        let mut broadcaster = EventBroadcaster::default();
        for _ in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let sender = Box::new(InputSender::new(Counter::inc, address));

            broadcaster.add(sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            block_on(broadcaster.broadcast(1)).unwrap();
        });

        let counter = Arc::new(AtomicUsize::new(0));

        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut counter = Counter::new(counter.clone());

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                Scheduler::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );
                        block_on(mailbox.recv(&mut counter, &dummy_context)).unwrap();
                    }
                })
            })
            .collect();

        th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::Relaxed), N_RECV);
    }

    #[test]
    fn broadcast_query_smoke() {
        const N_RECV: usize = 4;

        let mut mailboxes = Vec::new();
        let mut broadcaster = QueryBroadcaster::default();
        for _ in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let sender = Box::new(ReplierSender::new(Counter::fetch_inc, address));

            broadcaster.add(sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            let iter = block_on(broadcaster.broadcast(1)).unwrap();
            let sum = iter.fold(0, |acc, val| acc + val);

            assert_eq!(sum, N_RECV * (N_RECV - 1) / 2); // sum of {0, 1, 2, ..., (N_RECV - 1)}
        });

        let counter = Arc::new(AtomicUsize::new(0));

        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut counter = Counter::new(counter.clone());

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                Scheduler::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );
                        block_on(mailbox.recv(&mut counter, &dummy_context)).unwrap();
                        thread::sleep(std::time::Duration::from_millis(100));
                    }
                })
            })
            .collect();

        th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::Relaxed), N_RECV);
    }
}

#[cfg(all(test, asynchronix_loom))]
mod tests {
    use futures_channel::mpsc;
    use futures_util::StreamExt;

    use loom::model::Builder;
    use loom::sync::atomic::{AtomicBool, Ordering};
    use loom::thread;

    use waker_fn::waker_fn;

    use super::super::sender::SendError;
    use super::*;

    // An event that may be waken spuriously.
    struct TestEvent<R> {
        // The receiver is actually used only once in tests, so it is moved out
        // of the `Option` on first use.
        receiver: Option<mpsc::UnboundedReceiver<Option<R>>>,
    }
    impl<R: Send + 'static> Sender<(), R> for TestEvent<R> {
        fn send(&mut self, _arg: ()) -> Pin<Box<dyn Future<Output = Result<R, SendError>> + Send>> {
            let receiver = self.receiver.take().unwrap();

            Box::pin(async move {
                let mut stream = Box::pin(receiver.filter_map(|item| async { item }));

                Ok(stream.next().await.unwrap())
            })
        }
    }

    // An object that can wake a `TestEvent`.
    #[derive(Clone)]
    struct TestEventWaker<R> {
        sender: mpsc::UnboundedSender<Option<R>>,
    }
    impl<R> TestEventWaker<R> {
        fn wake_spurious(&self) {
            let _ = self.sender.unbounded_send(None);
        }
        fn wake_final(&self, value: R) {
            let _ = self.sender.unbounded_send(Some(value));
        }
    }

    fn test_event<R>() -> (TestEvent<R>, TestEventWaker<R>) {
        let (sender, receiver) = mpsc::unbounded();

        (
            TestEvent {
                receiver: Some(receiver),
            },
            TestEventWaker { sender },
        )
    }

    #[test]
    fn loom_broadcast_basic() {
        const DEFAULT_PREEMPTION_BOUND: usize = 3;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (test_event1, waker1) = test_event::<usize>();
            let (test_event2, waker2) = test_event::<usize>();
            let (test_event3, waker3) = test_event::<usize>();

            let mut broadcaster = QueryBroadcaster::default();
            broadcaster.add(Box::new(test_event1));
            broadcaster.add(Box::new(test_event2));
            broadcaster.add(Box::new(test_event3));

            let mut fut = Box::pin(broadcaster.broadcast(()));
            let is_scheduled = loom::sync::Arc::new(AtomicBool::new(false));
            let is_scheduled_waker = is_scheduled.clone();

            let waker = waker_fn(move || {
                // We use swap rather than a plain store to work around this
                // bug: <https://github.com/tokio-rs/loom/issues/254>
                is_scheduled_waker.swap(true, Ordering::Release);
            });
            let mut cx = Context::from_waker(&waker);

            let th1 = thread::spawn(move || waker1.wake_final(3));
            let th2 = thread::spawn(move || waker2.wake_final(7));
            let th3 = thread::spawn(move || waker3.wake_final(42));

            loop {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(Ok(mut res)) => {
                        assert_eq!(res.next(), Some(3));
                        assert_eq!(res.next(), Some(7));
                        assert_eq!(res.next(), Some(42));
                        assert_eq!(res.next(), None);

                        return;
                    }
                    Poll::Ready(Err(_)) => panic!("sender error"),
                    Poll::Pending => {}
                }

                // If the task has not been scheduled, exit the polling loop.
                if !is_scheduled.swap(false, Ordering::Acquire) {
                    break;
                }
            }

            th1.join().unwrap();
            th2.join().unwrap();
            th3.join().unwrap();

            assert!(is_scheduled.load(Ordering::Acquire));

            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(Ok(mut res)) => {
                    assert_eq!(res.next(), Some(3));
                    assert_eq!(res.next(), Some(7));
                    assert_eq!(res.next(), Some(42));
                    assert_eq!(res.next(), None);
                }
                Poll::Ready(Err(_)) => panic!("sender error"),
                Poll::Pending => panic!("the future has not completed"),
            };
        });
    }

    #[test]
    fn loom_broadcast_spurious() {
        const DEFAULT_PREEMPTION_BOUND: usize = 3;

        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
        }

        builder.check(move || {
            let (test_event1, waker1) = test_event::<usize>();
            let (test_event2, waker2) = test_event::<usize>();

            let mut broadcaster = QueryBroadcaster::default();
            broadcaster.add(Box::new(test_event1));
            broadcaster.add(Box::new(test_event2));

            let mut fut = Box::pin(broadcaster.broadcast(()));
            let is_scheduled = loom::sync::Arc::new(AtomicBool::new(false));
            let is_scheduled_waker = is_scheduled.clone();

            let waker = waker_fn(move || {
                // We use swap rather than a plain store to work around this
                // bug: <https://github.com/tokio-rs/loom/issues/254>
                is_scheduled_waker.swap(true, Ordering::Release);
            });
            let mut cx = Context::from_waker(&waker);

            let spurious_waker = waker1.clone();
            let th1 = thread::spawn(move || waker1.wake_final(3));
            let th2 = thread::spawn(move || waker2.wake_final(7));
            let th_spurious = thread::spawn(move || spurious_waker.wake_spurious());

            loop {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(Ok(mut res)) => {
                        assert_eq!(res.next(), Some(3));
                        assert_eq!(res.next(), Some(7));
                        assert_eq!(res.next(), None);

                        return;
                    }
                    Poll::Ready(Err(_)) => panic!("sender error"),
                    Poll::Pending => {}
                }

                // If the task has not been scheduled, exit the polling loop.
                if !is_scheduled.swap(false, Ordering::Acquire) {
                    break;
                }
            }

            th1.join().unwrap();
            th2.join().unwrap();
            th_spurious.join().unwrap();

            assert!(is_scheduled.load(Ordering::Acquire));

            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(Ok(mut res)) => {
                    assert_eq!(res.next(), Some(3));
                    assert_eq!(res.next(), Some(7));
                    assert_eq!(res.next(), None);
                }
                Poll::Ready(Err(_)) => panic!("sender error"),
                Poll::Pending => panic!("the future has not completed"),
            };
        });
    }
}
