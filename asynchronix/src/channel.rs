//! Multiple-producer single-consumer Channel for communication between
//! simulation models.
#![warn(missing_docs, missing_debug_implementations, unreachable_pub)]

mod queue;

use std::error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::sync::Arc;

use async_event::Event;
use diatomic_waker::primitives::DiatomicWaker;
use recycle_box::RecycleBox;

use queue::{PopError, PushError, Queue};
use recycle_box::coerce_box;

use crate::model::Model;
use crate::time::Scheduler;

/// Data shared between the receiver and the senders.
struct Inner<M> {
    /// Non-blocking internal queue.
    queue: Queue<dyn MessageFn<M>>,
    /// Signalling primitive used to notify the receiver.
    receiver_signal: DiatomicWaker,
    /// Signalling primitive used to notify one or several senders.
    sender_signal: Event,
    /// Current count of live senders.
    sender_count: AtomicUsize,
}

impl<M: 'static> Inner<M> {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Queue::new(capacity),
            receiver_signal: DiatomicWaker::new(),
            sender_signal: Event::new(),
            sender_count: AtomicUsize::new(0),
        }
    }
}

/// A receiver which can asynchronously execute `async` message that take an
/// argument of type `&mut M` and an optional `&Scheduler<M>` argument.
pub(crate) struct Receiver<M> {
    /// Shared data.
    inner: Arc<Inner<M>>,
    /// A recyclable box to temporarily store the `async` closure to be executed.
    future_box: Option<RecycleBox<()>>,
}

impl<M: Model> Receiver<M> {
    /// Creates a new receiver with the specified capacity.
    ///
    /// # Panic
    ///
    /// The constructor will panic if the requested capacity is 0 or is greater
    /// than `usize::MAX/2 + 1`.
    pub(crate) fn new(capacity: usize) -> Self {
        let inner = Arc::new(Inner::new(capacity));

        Receiver {
            inner,
            future_box: Some(RecycleBox::new(())),
        }
    }

    /// Creates a new sender.
    pub(crate) fn sender(&self) -> Sender<M> {
        // Increase the reference count of senders.
        //
        // Ordering: Relaxed ordering is sufficient here for the same reason it
        // is sufficient for an `Arc` reference count increment: synchronization
        // is only necessary when decrementing the counter since all what is
        // needed is to ensure that all operations until the drop handler is
        // called are visible once the reference count drops to 0.
        self.inner.sender_count.fetch_add(1, Ordering::Relaxed);

        Sender {
            inner: self.inner.clone(),
        }
    }

    /// Receives and executes a message asynchronously, if necessary waiting
    /// until one becomes available.
    pub(crate) async fn recv(
        &mut self,
        model: &mut M,
        scheduler: &Scheduler<M>,
    ) -> Result<(), RecvError> {
        let msg = unsafe {
            self.inner
                .receiver_signal
                .wait_until(|| match self.inner.queue.pop() {
                    Ok(msg) => Some(Some(msg)),
                    Err(PopError::Empty) => None,
                    Err(PopError::Closed) => Some(None),
                })
                .await
        };

        match msg {
            Some(mut msg) => {
                // Consume the message to obtain a boxed future.
                let fut = msg.call_once(model, scheduler, self.future_box.take().unwrap());

                // Now that `msg` was consumed and its slot in the queue was
                // freed, signal to one awaiting sender that one slot is
                // available for sending.
                self.inner.sender_signal.notify_one();

                // Await the future provided by the message.
                let mut fut = RecycleBox::into_pin(fut);
                fut.as_mut().await;

                // Recycle the box.
                self.future_box = Some(RecycleBox::vacate_pinned(fut));

                Ok(())
            }
            None => Err(RecvError),
        }
    }

    /// Closes the channel.
    ///
    /// This prevents any further messages from being sent to the channel.
    /// Messages that were already sent can still be received, however, which is
    /// why a call to this method should typically be followed by a loop
    /// receiving all remaining messages.
    ///
    /// For this reason, no counterpart to [`Sender::is_closed`] is exposed by
    /// the receiver as such method could easily be misused and lead to lost
    /// messages. Instead, messages should be received until a [`RecvError`] is
    /// returned.
    #[allow(unused)]
    pub(crate) fn close(&self) {
        if !self.inner.queue.is_closed() {
            self.inner.queue.close();

            // Notify all blocked senders that the channel is closed.
            self.inner.sender_signal.notify_all();
        }
    }

    /// Returns a unique identifier for the channel.
    ///
    /// All channels are guaranteed to have different identifiers at any given
    /// time, but an identifier may be reused after all handles to a channel
    /// have been dropped.
    pub(crate) fn channel_id(&self) -> ChannelId {
        ChannelId(NonZeroUsize::new(&*self.inner as *const Inner<M> as usize).unwrap())
    }
}

impl<M> Drop for Receiver<M> {
    fn drop(&mut self) {
        self.inner.queue.close();

        // Notify all blocked senders that the channel is closed.
        self.inner.sender_signal.notify_all();
    }
}

impl<M> fmt::Debug for Receiver<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

/// A handle to a channel that can send messages.
///
/// Multiple [`Sender`] handles can be created using the [`Receiver::sender`]
/// method or via cloning.
pub(crate) struct Sender<M: 'static> {
    /// Shared data.
    inner: Arc<Inner<M>>,
}

impl<M: Model> Sender<M> {
    /// Sends a message, if necessary waiting until enough capacity becomes
    /// available in the channel.
    pub(crate) async fn send<F>(&self, msg_fn: F) -> Result<(), SendError>
    where
        F: for<'a> FnOnce(
                &'a mut M,
                &'a Scheduler<M>,
                RecycleBox<()>,
            ) -> RecycleBox<dyn Future<Output = ()> + Send + 'a>
            + Send
            + 'static,
    {
        // Define a closure that boxes the argument in a type-erased
        // `RecycleBox`.
        let mut msg_fn = Some(|vacated_box| -> RecycleBox<dyn MessageFn<M>> {
            coerce_box!(RecycleBox::recycle(vacated_box, MessageFnOnce::new(msg_fn)))
        });

        let success = self
            .inner
            .sender_signal
            .wait_until(|| {
                match self.inner.queue.push(msg_fn.take().unwrap()) {
                    Ok(()) => Some(true),
                    Err(PushError::Full(m)) => {
                        // Recycle the message.
                        msg_fn = Some(m);

                        None
                    }
                    Err(PushError::Closed) => Some(false),
                }
            })
            .await;

        if success {
            self.inner.receiver_signal.notify();

            Ok(())
        } else {
            Err(SendError)
        }
    }

    /// Closes the channel.
    ///
    /// This prevents any further messages from being sent. Messages that were
    /// already sent can still be received.
    #[allow(unused)]
    pub(crate) fn close(&self) {
        self.inner.queue.close();

        // Notify the receiver and all blocked senders that the channel is
        // closed.
        self.inner.receiver_signal.notify();
        self.inner.sender_signal.notify_all();
    }

    /// Checks if the channel is closed.
    ///
    /// This can happen either because the [`Receiver`] was dropped or because
    /// one of the [`Sender::close`] or [`Receiver::close`] method was called.
    #[allow(unused)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.queue.is_closed()
    }

    /// Returns a unique identifier for the channel.
    ///
    /// All channels are guaranteed to have different identifiers at any given
    /// time, but an identifier may be reused after all handles to a channel
    /// have been dropped.
    pub(crate) fn channel_id(&self) -> ChannelId {
        ChannelId(NonZeroUsize::new(&*self.inner as *const Inner<M> as usize).unwrap())
    }
}

impl<M> Clone for Sender<M> {
    fn clone(&self) -> Self {
        // Increase the reference count of senders.
        //
        // Ordering: Relaxed ordering is sufficient here for the same reason it
        // is sufficient for an `Arc` reference count increment: synchronization
        // is only necessary when decrementing the counter since all what is
        // needed is to ensure that all operations until the drop handler is
        // called are visible once the reference count drops to 0.
        self.inner.sender_count.fetch_add(1, Ordering::Relaxed);

        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: 'static> Drop for Sender<M> {
    fn drop(&mut self) {
        // Decrease the reference count of senders.
        //
        // Ordering: Release ordering is necessary for the same reason it is
        // necessary for an `Arc` reference count decrement: it ensures that all
        // operations performed by this sender before it was dropped will be
        // visible once the sender count drops to 0.
        if self.inner.sender_count.fetch_sub(1, Ordering::Release) == 1
            && !self.inner.queue.is_closed()
        {
            // Make sure that the notified receiver sees all operations
            // performed by all dropped senders.
            //
            // Ordering: Acquire is necessary to synchronize with the Release
            // decrement operations. Note that the fence synchronizes with _all_
            // decrement operations since the chain of counter decrements forms
            // a Release sequence.
            atomic::fence(Ordering::Acquire);

            self.inner.queue.close();

            // Notify the senders that the channel is closed.
            self.inner.receiver_signal.notify();
        }
    }
}

impl<M> fmt::Debug for Sender<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Address").finish_non_exhaustive()
    }
}

/// A closure that can be called once to create a future boxed in a `RecycleBox`
/// from an `&mut M`, a `&Scheduler<M>` and an empty `RecycleBox`.
///
/// This is basically a workaround to emulate an `FnOnce` with the equivalent of
/// an `FnMut` so that it is possible to call it as a `dyn` trait stored in a
/// custom pointer type like `RecycleBox` (a `Box<dyn FnOnce>` would not need
/// this because it implements the magical `DerefMove` trait and therefore can
/// be used to call an `FnOnce`).
trait MessageFn<M: Model>: Send {
    /// A method that can be executed once.
    ///
    /// # Panics
    ///
    /// This method may panic if called more than once.
    fn call_once<'a>(
        &mut self,
        model: &'a mut M,
        scheduler: &'a Scheduler<M>,
        recycle_box: RecycleBox<()>,
    ) -> RecycleBox<dyn Future<Output = ()> + Send + 'a>;
}

/// A `MessageFn` implementation wrapping an async `FnOnce`.
struct MessageFnOnce<F, M> {
    msg_fn: Option<F>,
    _phantom: PhantomData<fn(&mut M)>,
}
impl<F, M> MessageFnOnce<F, M> {
    fn new(msg_fn: F) -> Self {
        Self {
            msg_fn: Some(msg_fn),
            _phantom: PhantomData,
        }
    }
}
impl<F, M: Model> MessageFn<M> for MessageFnOnce<F, M>
where
    F: for<'a> FnOnce(
            &'a mut M,
            &'a Scheduler<M>,
            RecycleBox<()>,
        ) -> RecycleBox<dyn Future<Output = ()> + Send + 'a>
        + Send,
{
    fn call_once<'a>(
        &mut self,
        model: &'a mut M,
        scheduler: &'a Scheduler<M>,
        recycle_box: RecycleBox<()>,
    ) -> RecycleBox<dyn Future<Output = ()> + Send + 'a> {
        let closure = self.msg_fn.take().unwrap();

        (closure)(model, scheduler, recycle_box)
    }
}

/// Unique identifier for a channel.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChannelId(NonZeroUsize);

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An error returned when an attempt to send a message asynchronously is
/// unsuccessful.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SendError;

/// An error returned when an attempt to receive a message asynchronously is
/// unsuccessful.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecvError;

impl error::Error for RecvError {}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "receiving from a closed channel".fmt(f)
    }
}
