use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::vec;

use pin_project::pin_project;

use diatomic_waker::WakeSink;

use super::sender::{Sender, SenderFuture};

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
    /// The list of senders with their associated line identifier.
    senders: Vec<Box<dyn Sender<T, R>>>,
}

impl<T: Clone, R> BroadcasterInner<T, R> {
    /// Adds a new sender associated to the specified identifier.
    ///
    /// # Panics
    ///
    /// This method will panic if the total count of senders would reach
    /// `u32::MAX - 1` due to limitations inherent to the task set
    /// implementation.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, R>>) {
        assert!(self.senders.len() < (u32::MAX as usize - 2));
        self.senders.push(sender);
    }

    /// Returns the number of connected senders.
    pub(super) fn len(&self) -> usize {
        self.senders.len()
    }

    /// Return a list of futures broadcasting an event or query to multiple
    /// addresses.
    fn futures(&mut self, arg: T) -> Vec<SenderFutureState<R>> {
        let mut future_states = Vec::new();

        // Broadcast the message and collect all futures.
        let mut iter = self.senders.iter_mut();
        while let Some(sender) = iter.next() {
            // Move the argument for the last future to avoid undue cloning.
            if iter.len() == 0 {
                if let Some(fut) = sender.send_owned(arg) {
                    future_states.push(SenderFutureState::Pending(fut));
                }
                break;
            }
            if let Some(fut) = sender.send(&arg) {
                future_states.push(SenderFutureState::Pending(fut));
            }
        }

        future_states
    }
}

impl<T: Clone, R> Default for BroadcasterInner<T, R> {
    fn default() -> Self {
        Self {
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
    /// `u32::MAX - 1` due to limitations inherent to the task set
    /// implementation.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, ()>>) {
        self.inner.add(sender);
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
            // One sender at most.
            [sender] => Fut::Single(sender.send_owned(arg)),
            // Possibly multiple senders.
            _ => Fut::Multiple(self.inner.futures(arg)),
        };

        async {
            match fut {
                // No sender.
                Fut::Empty | Fut::Single(None) => Ok(()),

                Fut::Single(Some(fut)) => fut.await.map_err(|_| BroadcastError {}),

                Fut::Multiple(mut futures) => match futures.as_mut_slice() {
                    // No sender.
                    [] => Ok(()),
                    // One sender.
                    [SenderFutureState::Pending(fut)] => fut.await.map_err(|_| BroadcastError {}),
                    // Multiple senders.
                    _ => BroadcastFuture::new(futures).await.map(|_| ()),
                },
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
    /// `u32::MAX - 1` due to limitations inherent to the task set
    /// implementation.
    pub(super) fn add(&mut self, sender: Box<dyn Sender<T, R>>) {
        self.inner.add(sender);
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
            // One sender at most.
            [sender] => Fut::Single(sender.send_owned(arg)),
            // Possibly multiple senders.
            _ => Fut::Multiple(self.inner.futures(arg)),
        };

        async {
            match fut {
                // No sender.
                Fut::Empty | Fut::Single(None) => Ok(ReplyIterator(Vec::new().into_iter())),

                Fut::Single(Some(fut)) => fut
                    .await
                    .map(|reply| ReplyIterator(vec![SenderFutureState::Ready(reply)].into_iter()))
                    .map_err(|_| BroadcastError {}),

                Fut::Multiple(mut futures) => match futures.as_mut_slice() {
                    // No sender.
                    [] => Ok(ReplyIterator(Vec::new().into_iter())),

                    // One sender.
                    [SenderFutureState::Pending(fut)] => fut
                        .await
                        .map(|reply| {
                            ReplyIterator(vec![SenderFutureState::Ready(reply)].into_iter())
                        })
                        .map_err(|_| BroadcastError {}),

                    // Multiple senders.
                    _ => BroadcastFuture::new(futures)
                        .await
                        .map_err(|_| BroadcastError {}),
                },
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

#[pin_project]
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
    use crate::simulation::{Address, GlobalScheduler};
    use crate::time::{MonotonicTime, TearableAtomicTime};
    use crate::util::priority_queue::PriorityQueue;
    use crate::util::sync_cell::SyncCell;

    use super::super::sender::{
        FilterMapInputSender, FilterMapReplierSender, InputSender, ReplierSender,
    };
    use super::*;
    use crate::model::{Context, Model};

    struct SumModel {
        inner: Arc<AtomicUsize>,
    }
    impl SumModel {
        fn new(counter: Arc<AtomicUsize>) -> Self {
            Self { inner: counter }
        }
        async fn increment(&mut self, by: usize) {
            self.inner.fetch_add(by, Ordering::Relaxed);
        }
    }
    impl Model for SumModel {}

    struct DoubleModel {}
    impl DoubleModel {
        fn new() -> Self {
            Self {}
        }
        async fn double(&mut self, value: usize) -> usize {
            2 * value
        }
    }
    impl Model for DoubleModel {}

    #[test]
    fn broadcast_event_smoke() {
        const N_RECV: usize = 4;
        const MESSAGE: usize = 42;

        let mut mailboxes = Vec::new();
        let mut broadcaster = EventBroadcaster::default();
        for _ in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let sender = Box::new(InputSender::new(SumModel::increment, address));

            broadcaster.add(sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            block_on(broadcaster.broadcast(MESSAGE)).unwrap();
        });

        let sum = Arc::new(AtomicUsize::new(0));

        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut sum_model = SumModel::new(sum.clone());

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let mut dummy_cx = Context::new(
                            String::new(),
                            GlobalScheduler::new(dummy_priority_queue, dummy_time),
                            Address(dummy_address),
                        );
                        block_on(mailbox.recv(&mut sum_model, &mut dummy_cx)).unwrap();
                    }
                })
            })
            .collect();

        th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(sum.load(Ordering::Relaxed), N_RECV * MESSAGE);
    }

    #[test]
    fn broadcast_event_filter_map() {
        const N_RECV: usize = 4;
        const BROADCAST_ALL: usize = 42; // special ID signaling that the message must reach all receivers.

        let mut mailboxes = Vec::new();
        let mut broadcaster = EventBroadcaster::default();
        for id in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let id_filter_sender = Box::new(FilterMapInputSender::new(
                move |x: &usize| (*x == id || *x == BROADCAST_ALL).then_some(*x),
                SumModel::increment,
                address,
            ));

            broadcaster.add(id_filter_sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            block_on(async {
                // Send messages reaching only one receiver each.
                for id in 0..N_RECV {
                    broadcaster.broadcast(id).await.unwrap();
                }

                // Broadcast the special value to all receivers.
                broadcaster.broadcast(BROADCAST_ALL).await.unwrap();

                // Send again messages reaching only one receiver each.
                for id in 0..N_RECV {
                    broadcaster.broadcast(id).await.unwrap();
                }
            })
        });

        let sum = Arc::new(AtomicUsize::new(0));

        // Spawn all models.
        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut sum_model = SumModel::new(sum.clone());

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let mut dummy_cx = Context::new(
                            String::new(),
                            GlobalScheduler::new(dummy_priority_queue, dummy_time),
                            Address(dummy_address),
                        );
                        block_on(async {
                            mailbox.recv(&mut sum_model, &mut dummy_cx).await.unwrap();
                            mailbox.recv(&mut sum_model, &mut dummy_cx).await.unwrap();
                            mailbox.recv(&mut sum_model, &mut dummy_cx).await.unwrap();
                        });
                    }
                })
            })
            .collect();

        th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(
            sum.load(Ordering::Relaxed),
            N_RECV * ((N_RECV - 1) + BROADCAST_ALL) // Twice the sum of all IDs + N_RECV times the special value
        );
    }

    #[test]
    fn broadcast_query_smoke() {
        const N_RECV: usize = 4;
        const MESSAGE: usize = 42;

        let mut mailboxes = Vec::new();
        let mut broadcaster = QueryBroadcaster::default();
        for _ in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let sender = Box::new(ReplierSender::new(DoubleModel::double, address));

            broadcaster.add(sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            let iter = block_on(broadcaster.broadcast(MESSAGE)).unwrap();
            let sum = iter.fold(0, |acc, val| acc + val);

            sum
        });

        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut double_model = DoubleModel::new();

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let mut dummy_cx = Context::new(
                            String::new(),
                            GlobalScheduler::new(dummy_priority_queue, dummy_time),
                            Address(dummy_address),
                        );
                        block_on(mailbox.recv(&mut double_model, &mut dummy_cx)).unwrap();
                        thread::sleep(std::time::Duration::from_millis(100));
                    }
                })
            })
            .collect();

        let sum = th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(sum, N_RECV * MESSAGE * 2);
    }

    #[test]
    fn broadcast_query_filter_map() {
        const N_RECV: usize = 4;
        const BROADCAST_ALL: usize = 42; // special ID signaling that the message must reach all receivers.

        let mut mailboxes = Vec::new();
        let mut broadcaster = QueryBroadcaster::default();
        for id in 0..N_RECV {
            let mailbox = Receiver::new(10);
            let address = mailbox.sender();
            let sender = Box::new(FilterMapReplierSender::new(
                move |x: &usize| (*x == id || *x == BROADCAST_ALL).then_some(*x),
                |x| 3 * x,
                DoubleModel::double,
                address,
            ));

            broadcaster.add(sender);
            mailboxes.push(mailbox);
        }

        let th_broadcast = thread::spawn(move || {
            block_on(async {
                let mut sum = 0;

                // Send messages reaching only one receiver each.
                for id in 0..N_RECV {
                    sum += broadcaster
                        .broadcast(id)
                        .await
                        .unwrap()
                        .fold(0, |acc, val| acc + val);
                }

                // Broadcast the special value to all receivers.
                sum += broadcaster
                    .broadcast(BROADCAST_ALL)
                    .await
                    .unwrap()
                    .fold(0, |acc, val| acc + val);

                // Send again messages reaching only one receiver each.
                for id in 0..N_RECV {
                    sum += broadcaster
                        .broadcast(id)
                        .await
                        .unwrap()
                        .fold(0, |acc, val| acc + val);
                }

                sum
            })
        });

        let th_recv: Vec<_> = mailboxes
            .into_iter()
            .map(|mut mailbox| {
                thread::spawn({
                    let mut double_model = DoubleModel::new();

                    move || {
                        let dummy_address = Receiver::new(1).sender();
                        let dummy_priority_queue = Arc::new(Mutex::new(PriorityQueue::new()));
                        let dummy_time =
                            SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH)).reader();
                        let mut dummy_cx = Context::new(
                            String::new(),
                            GlobalScheduler::new(dummy_priority_queue, dummy_time),
                            Address(dummy_address),
                        );

                        block_on(async {
                            mailbox
                                .recv(&mut double_model, &mut dummy_cx)
                                .await
                                .unwrap();
                            mailbox
                                .recv(&mut double_model, &mut dummy_cx)
                                .await
                                .unwrap();
                            mailbox
                                .recv(&mut double_model, &mut dummy_cx)
                                .await
                                .unwrap();
                        });
                        thread::sleep(std::time::Duration::from_millis(100));
                    }
                })
            })
            .collect();

        let sum = th_broadcast.join().unwrap();
        for th in th_recv {
            th.join().unwrap();
        }

        assert_eq!(
            sum,
            N_RECV * ((N_RECV - 1) + BROADCAST_ALL) * 2 * 3, // Twice the sum of all IDs + N_RECV times the special value, then doubled and tripled
        );
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
        fn send(
            &mut self,
            _arg: &(),
        ) -> Option<Pin<Box<dyn Future<Output = Result<R, SendError>> + Send>>> {
            let receiver = self.receiver.take().unwrap();

            Some(Box::pin(async move {
                let mut stream = Box::pin(receiver.filter_map(|item| async { item }));

                Ok(stream.next().await.unwrap())
            }))
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

    // This tests fails with "Concurrent load and mut accesses" even though the
    // `task_list` implementation which triggers it does not use any unsafe.
    // This is most certainly related to this Loom bug:
    //
    // https://github.com/tokio-rs/loom/issues/260
    //
    // Disabling until the bug is fixed.
    #[ignore]
    #[test]
    fn loom_broadcast_query_basic() {
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

    // This tests fails with "Concurrent load and mut accesses" even though the
    // `task_list` implementation which triggers it does not use any unsafe.
    // This is most certainly related to this Loom bug:
    //
    // https://github.com/tokio-rs/loom/issues/260
    //
    // Disabling until the bug is fixed.
    #[ignore]
    #[test]
    fn loom_broadcast_query_spurious() {
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
