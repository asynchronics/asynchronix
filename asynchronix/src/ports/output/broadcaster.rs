use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll};

use diatomic_waker::WakeSink;

use super::sender::{RecycledFuture, SendError, Sender};
use super::LineId;
use crate::util::task_set::TaskSet;

/// An object that can efficiently broadcast messages to several addresses.
///
/// This is very similar to `source::broadcaster::BroadcasterInner`, but
/// generates non-owned futures instead.
///
/// This object maintains a list of senders associated to each target address.
/// When a message is broadcast, the sender futures are awaited in parallel.
/// This is somewhat similar to what `FuturesOrdered` in the `futures` crate
/// does, but with some key differences:
///
/// - tasks, output storage and future storage are reusable to avoid repeated
///   allocation, so allocation occurs only after a new sender is added,
/// - the outputs of all sender futures are returned all at once rather than
///   with an asynchronous iterator (a.k.a. async stream).
pub(super) struct BroadcasterInner<T: Clone, R> {
    /// Line identifier for the next port to be connected.
    next_line_id: u64,
    /// The list of senders with their associated line identifier.
    senders: Vec<(LineId, Box<dyn Sender<T, R>>)>,
    /// Fields explicitly borrowed by the `BroadcastFuture`.
    shared: Shared<R>,
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
        self.shared.outputs.push(None);

        // The storage is alway an empty vector so we just book some capacity.
        if let Some(storage) = self.shared.storage.as_mut() {
            let _ = storage.try_reserve(self.senders.len());
        };

        line_id
    }

    /// Removes the first sender with the specified identifier, if any.
    ///
    /// Returns `true` if there was indeed a sender associated to the specified
    /// identifier.
    pub(super) fn remove(&mut self, id: LineId) -> bool {
        if let Some(pos) = self.senders.iter().position(|s| s.0 == id) {
            self.senders.swap_remove(pos);
            self.shared.outputs.truncate(self.senders.len());

            return true;
        }

        false
    }

    /// Removes all senders.
    pub(super) fn clear(&mut self) {
        self.senders.clear();
        self.shared.outputs.clear();
    }

    /// Returns the number of connected senders.
    pub(super) fn len(&self) -> usize {
        self.senders.len()
    }

    /// Return a list of futures broadcasting an event or query to multiple
    /// addresses.
    #[allow(clippy::type_complexity)]
    fn futures(
        &mut self,
        arg: T,
    ) -> (
        &'_ mut Shared<R>,
        Vec<RecycledFuture<'_, Result<R, SendError>>>,
    ) {
        let mut futures = recycle_vec(self.shared.storage.take().unwrap_or_default());

        // Broadcast the message and collect all futures.
        let mut iter = self.senders.iter_mut();
        while let Some(sender) = iter.next() {
            // Move the argument rather than clone it for the last future.
            if iter.len() == 0 {
                if let Some(fut) = sender.1.send_owned(arg) {
                    futures.push(fut);
                }
                break;
            }

            if let Some(fut) = sender.1.send(&arg) {
                futures.push(fut);
            }
        }

        (&mut self.shared, futures)
    }
}

impl<T: Clone, R> Default for BroadcasterInner<T, R> {
    /// Creates an empty `Broadcaster` object.
    fn default() -> Self {
        let wake_sink = WakeSink::new();
        let wake_src = wake_sink.source();

        Self {
            next_line_id: 0,
            senders: Vec::new(),
            shared: Shared {
                wake_sink,
                task_set: TaskSet::new(wake_src),
                outputs: Vec::new(),
                storage: None,
            },
        }
    }
}

impl<T: Clone, R> Clone for BroadcasterInner<T, R> {
    fn clone(&self) -> Self {
        Self {
            next_line_id: self.next_line_id,
            senders: self.senders.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// An object that can efficiently broadcast events to several input ports.
///
/// This is very similar to `source::broadcaster::EventBroadcaster`, but
/// generates non-owned futures instead.
///
/// See `BroadcasterInner` for implementation details.
#[derive(Clone)]
pub(super) struct EventBroadcaster<T: Clone> {
    /// The broadcaster core object.
    inner: BroadcasterInner<T, ()>,
}

impl<T: Clone> EventBroadcaster<T> {
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
    pub(super) async fn broadcast(&mut self, arg: T) -> Result<(), BroadcastError> {
        match self.inner.senders.as_mut_slice() {
            // No sender.
            [] => Ok(()),

            // One sender at most.
            [sender] => match sender.1.send_owned(arg) {
                None => Ok(()),
                Some(fut) => fut.await.map_err(|_| BroadcastError {}),
            },

            // Possibly multiple senders.
            _ => {
                let (shared, mut futures) = self.inner.futures(arg);
                match futures.as_mut_slice() {
                    [] => Ok(()),
                    [fut] => fut.await.map_err(|_| BroadcastError {}),
                    _ => BroadcastFuture::new(shared, futures).await,
                }
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
/// This is very similar to `source::broadcaster::QueryBroadcaster`, but
/// generates non-owned futures instead.
///
/// See `BroadcasterInner` for implementation details.
pub(super) struct QueryBroadcaster<T: Clone, R> {
    /// The broadcaster core object.
    inner: BroadcasterInner<T, R>,
}

impl<T: Clone, R> QueryBroadcaster<T, R> {
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

    /// Broadcasts a query to all addresses and collect all responses.
    pub(super) async fn broadcast(
        &mut self,
        arg: T,
    ) -> Result<impl Iterator<Item = R> + '_, BroadcastError> {
        let output_count = match self.inner.senders.as_mut_slice() {
            // No sender.
            [] => 0,

            // One sender at most.
            [sender] => {
                if let Some(fut) = sender.1.send_owned(arg) {
                    let output = fut.await.map_err(|_| BroadcastError {})?;
                    self.inner.shared.outputs[0] = Some(output);

                    1
                } else {
                    0
                }
            }

            // Possibly multiple senders.
            _ => {
                let (shared, mut futures) = self.inner.futures(arg);
                let output_count = futures.len();

                match futures.as_mut_slice() {
                    [] => {}
                    [fut] => {
                        let output = fut.await.map_err(|_| BroadcastError {})?;
                        shared.outputs[0] = Some(output);
                    }
                    _ => {
                        BroadcastFuture::new(shared, futures).await?;
                    }
                }

                output_count
            }
        };

        // At this point all outputs should be available.
        let outputs = self
            .inner
            .shared
            .outputs
            .iter_mut()
            .take(output_count)
            .map(|t| t.take().unwrap());

        Ok(outputs)
    }
}

impl<T: Clone, R> Default for QueryBroadcaster<T, R> {
    fn default() -> Self {
        Self {
            inner: BroadcasterInner::default(),
        }
    }
}

impl<T: Clone, R> Clone for QueryBroadcaster<T, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Fields of `Broadcaster` that are explicitly borrowed by a `BroadcastFuture`.
struct Shared<R> {
    /// Thread-safe waker handle.
    wake_sink: WakeSink,
    /// Tasks associated to the sender futures.
    task_set: TaskSet,
    /// Outputs of the sender futures.
    outputs: Vec<Option<R>>,
    /// Cached storage for the sender futures.
    ///
    /// When it exists, the cached storage is always an empty vector but it
    /// typically has a non-zero capacity. Its purpose is to reuse the
    /// previously allocated capacity when creating new sender futures.
    storage: Option<Vec<Pin<RecycledFuture<'static, R>>>>,
}

impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        let wake_sink = WakeSink::new();
        let wake_src = wake_sink.source();

        let mut outputs = Vec::new();
        outputs.resize_with(self.outputs.len(), Default::default);

        Self {
            wake_sink,
            task_set: TaskSet::new(wake_src),
            outputs,
            storage: None,
        }
    }
}

/// A future aggregating the outputs of a collection of sender futures.
///
/// The idea is to join all sender futures as efficiently as possible, meaning:
///
/// - the sender futures are polled simultaneously rather than waiting for their
///   completion in a sequential manner,
/// - the storage allocated for the sender futures is always returned to the
///   `Broadcast` object so it can be reused by the next future,
/// - the happy path (all futures immediately ready) is very fast.
pub(super) struct BroadcastFuture<'a, R> {
    /// Reference to the shared fields of the `Broadcast` object.
    shared: &'a mut Shared<R>,
    /// List of all send futures.
    futures: ManuallyDrop<Vec<RecycledFuture<'a, Result<R, SendError>>>>,
    /// The total count of futures that have not yet been polled to completion.
    pending_futures_count: usize,
    /// State of completion of the future.
    state: FutureState,
}

impl<'a, R> BroadcastFuture<'a, R> {
    /// Creates a new `BroadcastFuture`.
    fn new(
        shared: &'a mut Shared<R>,
        futures: Vec<RecycledFuture<'a, Result<R, SendError>>>,
    ) -> Self {
        let pending_futures_count = futures.len();
        shared.task_set.resize(pending_futures_count);

        for output in shared.outputs.iter_mut().take(pending_futures_count) {
            // Empty the output slots to be used. This is necessary in case the
            // previous broadcast future was cancelled.
            output.take();
        }

        BroadcastFuture {
            shared,
            futures: ManuallyDrop::new(futures),
            state: FutureState::Uninit,
            pending_futures_count,
        }
    }
}

impl<'a, R> Drop for BroadcastFuture<'a, R> {
    fn drop(&mut self) {
        // Safety: this is safe since `self.futures` is never accessed after it
        // is moved out.
        let futures = unsafe { ManuallyDrop::take(&mut self.futures) };

        // Recycle the vector that contained the futures.
        self.shared.storage = Some(recycle_vec(futures));
    }
}

impl<'a, R> Future for BroadcastFuture<'a, R> {
    type Output = Result<(), BroadcastError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        assert_ne!(
            this.state,
            FutureState::Completed,
            "broadcast future polled after completion"
        );

        // Poll all sender futures once if this is the first time the broadcast
        // future is polled.
        if this.state == FutureState::Uninit {
            // The task set is re-used for each broadcast, so it may have some
            // task scheduled due to e.g. spurious wake-ups that were triggered
            // after the previous broadcast was completed. Discarding scheduled
            // tasks can prevent unnecessary wake-ups.
            this.shared.task_set.discard_scheduled();

            for task_idx in 0..this.futures.len() {
                let output = &mut this.shared.outputs[task_idx];
                let future = std::pin::Pin::new(&mut this.futures[task_idx]);
                let task_waker_ref = this.shared.task_set.waker_of(task_idx);
                let task_cx_ref = &mut Context::from_waker(&task_waker_ref);

                match future.poll(task_cx_ref) {
                    Poll::Ready(Ok(o)) => {
                        *output = Some(o);
                        this.pending_futures_count -= 1;
                    }
                    Poll::Ready(Err(_)) => {
                        this.state = FutureState::Completed;

                        return Poll::Ready(Err(BroadcastError {}));
                    }
                    Poll::Pending => {}
                }
            }

            if this.pending_futures_count == 0 {
                this.state = FutureState::Completed;

                return Poll::Ready(Ok(()));
            }

            this.state = FutureState::Pending;
        }

        // Repeatedly poll the futures of all scheduled tasks until there are no
        // more scheduled tasks.
        loop {
            // No need to register the waker if some tasks have been scheduled.
            if !this.shared.task_set.has_scheduled() {
                this.shared.wake_sink.register(cx.waker());
            }

            // Retrieve the indices of the scheduled tasks if any. If there are
            // no scheduled tasks, `Poll::Pending` is returned and this future
            // will be awaken again when enough tasks have been awaken.
            //
            // NOTE: the current implementation requires a notification to be
            // sent each time a sub-future has made progress. We may try at some
            // point to benchmark an alternative strategy where a notification
            // is requested only when all pending sub-futures have made progress,
            // using `take_scheduled(this.pending_futures_count)`. This would
            // reduce the cost of context switch but could hurt latency.
            let scheduled_tasks = match this.shared.task_set.take_scheduled(1) {
                Some(st) => st,
                None => return Poll::Pending,
            };

            for task_idx in scheduled_tasks {
                let output = &mut this.shared.outputs[task_idx];

                // Do not poll completed futures.
                if output.is_some() {
                    continue;
                }

                let future = std::pin::Pin::new(&mut this.futures[task_idx]);
                let task_waker_ref = this.shared.task_set.waker_of(task_idx);
                let task_cx_ref = &mut Context::from_waker(&task_waker_ref);

                match future.poll(task_cx_ref) {
                    Poll::Ready(Ok(o)) => {
                        *output = Some(o);
                        this.pending_futures_count -= 1;
                    }
                    Poll::Ready(Err(_)) => {
                        this.state = FutureState::Completed;

                        return Poll::Ready(Err(BroadcastError {}));
                    }
                    Poll::Pending => {}
                }
            }

            if this.pending_futures_count == 0 {
                this.state = FutureState::Completed;

                return Poll::Ready(Ok(()));
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

/// Drops all items in a vector and returns an empty vector of another type,
/// preserving the allocation and capacity of the original vector provided that
/// the layouts of `T` and `U` are compatible.
///
/// # Panics
///
/// This will panic in debug mode if the layouts are incompatible.
fn recycle_vec<T, U>(mut v: Vec<T>) -> Vec<U> {
    debug_assert_eq!(
        std::alloc::Layout::new::<T>(),
        std::alloc::Layout::new::<U>()
    );

    let cap = v.capacity();

    // No unsafe here: this just relies on an optimization in the `collect`
    // method.
    v.clear();
    let v_out: Vec<U> = v.into_iter().map(|_| unreachable!()).collect();

    debug_assert_eq!(v_out.capacity(), cap);

    v_out
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use futures_executor::block_on;

    use crate::channel::Receiver;
    use crate::simulation::{Address, LocalScheduler, SchedulerInner};
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
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                SchedulerInner::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );
                        block_on(mailbox.recv(&mut sum_model, &dummy_context)).unwrap();
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
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                SchedulerInner::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );
                        block_on(async {
                            mailbox.recv(&mut sum_model, &dummy_context).await.unwrap();
                            mailbox.recv(&mut sum_model, &dummy_context).await.unwrap();
                            mailbox.recv(&mut sum_model, &dummy_context).await.unwrap();
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
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                SchedulerInner::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );
                        block_on(mailbox.recv(&mut double_model, &dummy_context)).unwrap();
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
                        let dummy_context = Context::new(
                            String::new(),
                            LocalScheduler::new(
                                SchedulerInner::new(dummy_priority_queue, dummy_time),
                                Address(dummy_address),
                            ),
                        );

                        block_on(async {
                            mailbox
                                .recv(&mut double_model, &dummy_context)
                                .await
                                .unwrap();
                            mailbox
                                .recv(&mut double_model, &dummy_context)
                                .await
                                .unwrap();
                            mailbox
                                .recv(&mut double_model, &dummy_context)
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

    use recycle_box::RecycleBox;
    use waker_fn::waker_fn;

    use super::super::sender::RecycledFuture;
    use super::*;

    // An event that may be waken spuriously.
    struct TestEvent<R> {
        receiver: mpsc::UnboundedReceiver<Option<R>>,
        fut_storage: Option<RecycleBox<()>>,
    }
    impl<R: Send> Sender<(), R> for TestEvent<R> {
        fn send(&mut self, _arg: &()) -> Option<RecycledFuture<'_, Result<R, SendError>>> {
            let fut_storage = &mut self.fut_storage;
            let receiver = &mut self.receiver;

            Some(RecycledFuture::new(fut_storage, async {
                let mut stream = Box::pin(receiver.filter_map(|item| async { item }));

                Ok(stream.next().await.unwrap())
            }))
        }
    }

    impl<R> Clone for TestEvent<R> {
        fn clone(&self) -> Self {
            unreachable!()
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
                receiver,
                fut_storage: None,
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

    // This tests fails with "Concurrent load and mut accesses" even though the
    // `task_list` implementation which triggers it does not use any unsafe.
    // This is most certainly related to this Loom bug:
    //
    // https://github.com/tokio-rs/loom/issues/260
    //
    // Disabling until the bug is fixed.
    #[ignore]
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
