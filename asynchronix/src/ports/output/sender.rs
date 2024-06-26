use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll};

use dyn_clone::DynClone;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel;
use crate::model::Model;
use crate::ports::{EventSinkWriter, InputFn, ReplierFn};

/// An event or query sender abstracting over the target model and input or
/// replier method.
pub(super) trait Sender<T, R>: DynClone + Send {
    /// Asynchronously send the event or request.
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<R, SendError>>;
}

dyn_clone::clone_trait_object!(<T, R> Sender<T, R>);

/// An object that can send events to an input port.
pub(super) struct InputSender<M: 'static, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + 'static,
{
    func: F,
    sender: channel::Sender<M>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_closure: PhantomData<fn(&mut M, T)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M: Send, F, T, S> InputSender<M, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + 'static,
{
    pub(super) fn new(func: F, sender: channel::Sender<M>) -> Self {
        Self {
            func,
            sender,
            fut_storage: None,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M: Send, F, T, S> Sender<T, ()> for InputSender<M, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Send + 'static,
    S: Send + 'static,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let func = self.func.clone();

        let fut = self.sender.send(move |model, scheduler, recycle_box| {
            let fut = func.call(model, arg, scheduler);

            coerce_box!(RecycleBox::recycle(recycle_box, fut))
        });

        RecycledFuture::new(&mut self.fut_storage, async move {
            fut.await.map_err(|_| SendError {})
        })
    }
}

impl<M: Send, F, T, S> Clone for InputSender<M, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Send + 'static,
    S: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
            sender: self.sender.clone(),
            fut_storage: None,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

/// An object that can send a request to a replier port and retrieve a response.
pub(super) struct ReplierSender<M: 'static, F, T, R, S> {
    func: F,
    sender: channel::Sender<M>,
    receiver: multishot::Receiver<R>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_closure: PhantomData<fn(&mut M, T) -> R>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, F, T, R, S> ReplierSender<M, F, T, R, S>
where
    M: Model,
    F: for<'a> ReplierFn<'a, M, T, R, S>,
    T: Send + 'static,
    R: Send + 'static,
{
    pub(super) fn new(func: F, sender: channel::Sender<M>) -> Self {
        Self {
            func,
            sender,
            receiver: multishot::Receiver::new(),
            fut_storage: None,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, F, T, R, S> Sender<T, R> for ReplierSender<M, F, T, R, S>
where
    M: Model,
    F: for<'a> ReplierFn<'a, M, T, R, S> + Clone,
    T: Send + 'static,
    R: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<R, SendError>> {
        let func = self.func.clone();
        let sender = &mut self.sender;
        let reply_receiver = &mut self.receiver;
        let fut_storage = &mut self.fut_storage;

        // The previous future generated by this method should have been polled
        // to completion so a new sender should be readily available.
        let reply_sender = reply_receiver.sender().unwrap();

        let send_fut = sender.send(move |model, scheduler, recycle_box| {
            let fut = async move {
                let reply = func.call(model, arg, scheduler).await;
                reply_sender.send(reply);
            };

            coerce_box!(RecycleBox::recycle(recycle_box, fut))
        });

        RecycledFuture::new(fut_storage, async move {
            // Send the message.
            send_fut.await.map_err(|_| SendError {})?;

            // Wait until the message is processed and the reply is sent back.
            // If an error is received, it most likely means the mailbox was
            // dropped before the message was processed.
            reply_receiver.recv().await.map_err(|_| SendError {})
        })
    }
}

impl<M, F, T, R, S> Clone for ReplierSender<M, F, T, R, S>
where
    M: Model,
    F: for<'a> ReplierFn<'a, M, T, R, S> + Clone,
    T: Send + 'static,
    R: Send + 'static,
    S: Send,
{
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
            sender: self.sender.clone(),
            receiver: multishot::Receiver::new(),
            fut_storage: None,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

/// An object that can send a payload to an event sink.
pub(super) struct EventSinkSender<T: Send + 'static, W: EventSinkWriter<T>> {
    writer: W,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_event: PhantomData<T>,
}

impl<T: Send + 'static, W: EventSinkWriter<T>> EventSinkSender<T, W> {
    pub(super) fn new(writer: W) -> Self {
        Self {
            writer,
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

impl<T, W> Sender<T, ()> for EventSinkSender<T, W>
where
    T: Send + 'static,
    W: EventSinkWriter<T>,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let writer = &mut self.writer;

        RecycledFuture::new(&mut self.fut_storage, async move {
            writer.write(arg);

            Ok(())
        })
    }
}

impl<T, W> Clone for EventSinkSender<T, W>
where
    T: Send + 'static,
    W: EventSinkWriter<T>,
{
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

/// Error returned when the mailbox was closed or dropped.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) struct SendError {}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending message into a closed mailbox")
    }
}

impl Error for SendError {}

pub(super) struct RecycledFuture<'a, T> {
    fut: ManuallyDrop<Pin<RecycleBox<dyn Future<Output = T> + Send + 'a>>>,
    lender_box: &'a mut Option<RecycleBox<()>>,
}
impl<'a, T> RecycledFuture<'a, T> {
    pub(super) fn new<F: Future<Output = T> + Send + 'a>(
        lender_box: &'a mut Option<RecycleBox<()>>,
        fut: F,
    ) -> Self {
        let vacated_box = lender_box.take().unwrap_or_else(|| RecycleBox::new(()));
        let fut: RecycleBox<dyn Future<Output = T> + Send + 'a> =
            coerce_box!(RecycleBox::recycle(vacated_box, fut));

        Self {
            fut: ManuallyDrop::new(RecycleBox::into_pin(fut)),
            lender_box,
        }
    }
}

impl<'a, T> Drop for RecycledFuture<'a, T> {
    fn drop(&mut self) {
        // Return the box to the lender.
        //
        // Safety: taking the `fut` member is safe since it is never used again.
        *self.lender_box = Some(RecycleBox::vacate_pinned(unsafe {
            ManuallyDrop::take(&mut self.fut)
        }));
    }
}

impl<'a, T> Future for RecycledFuture<'a, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.as_mut().poll(cx)
    }
}
