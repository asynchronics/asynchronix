use std::error::Error;
use std::fmt;
use std::future::{ready, Future};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
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
pub(super) struct InputSender<M, F, T, S>
where
    M: 'static,
{
    func: F,
    sender: channel::Sender<M>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_closure: PhantomData<fn(&mut M, T)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, F, T, S> InputSender<M, F, T, S>
where
    M: 'static,
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

impl<M, F, T, S> Sender<T, ()> for InputSender<M, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Send + 'static,
    S: Send,
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

impl<M, F, T, S> Clone for InputSender<M, F, T, S>
where
    M: 'static,
    F: Clone,
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

/// An object that can send mapped events to an input port.
pub(super) struct MapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    map: Arc<C>,
    func: F,
    sender: channel::Sender<M>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_map: PhantomData<fn(T) -> U>,
    _phantom_closure: PhantomData<fn(&mut M, U)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, F, T, U, S> MapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    pub(super) fn new(map: C, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            map: Arc::new(map),
            func,
            sender,
            fut_storage: None,
            _phantom_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, F, T, U, S> Sender<T, ()> for MapInputSender<M, C, F, T, U, S>
where
    M: Model,
    C: Fn(T) -> U + Send + Sync,
    F: for<'a> InputFn<'a, M, U, S> + Clone,
    T: Send + 'static,
    U: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let func = self.func.clone();
        let arg = (self.map)(arg);

        let fut = self.sender.send(move |model, scheduler, recycle_box| {
            let fut = func.call(model, arg, scheduler);

            coerce_box!(RecycleBox::recycle(recycle_box, fut))
        });

        RecycledFuture::new(&mut self.fut_storage, async move {
            fut.await.map_err(|_| SendError {})
        })
    }
}

impl<M, C, F, T, U, S> Clone for MapInputSender<M, C, F, T, U, S>
where
    M: 'static,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            func: self.func.clone(),
            sender: self.sender.clone(),
            fut_storage: None,
            _phantom_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

/// An object that can filter and send mapped events to an input port.
pub(super) struct FilterMapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    filter_map: Arc<C>,
    func: F,
    sender: channel::Sender<M>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_filter_map: PhantomData<fn(T) -> Option<U>>,
    _phantom_closure: PhantomData<fn(&mut M, U)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, F, T, U, S> FilterMapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    pub(super) fn new(filter_map: C, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            filter_map: Arc::new(filter_map),
            func,
            sender,
            fut_storage: None,
            _phantom_filter_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, F, T, U, S> Sender<T, ()> for FilterMapInputSender<M, C, F, T, U, S>
where
    M: Model,
    C: Fn(T) -> Option<U> + Send + Sync,
    F: for<'a> InputFn<'a, M, U, S> + Clone,
    T: Send + 'static,
    U: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let func = self.func.clone();

        match (self.filter_map)(arg) {
            Some(arg) => {
                let fut = self.sender.send(move |model, scheduler, recycle_box| {
                    let fut = func.call(model, arg, scheduler);

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                });

                RecycledFuture::new(&mut self.fut_storage, async move {
                    fut.await.map_err(|_| SendError {})
                })
            }
            None => RecycledFuture::new(&mut self.fut_storage, ready(Ok(()))),
        }
    }
}

impl<M, C, F, T, U, S> Clone for FilterMapInputSender<M, C, F, T, U, S>
where
    M: 'static,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            filter_map: self.filter_map.clone(),
            func: self.func.clone(),
            sender: self.sender.clone(),
            fut_storage: None,
            _phantom_filter_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

/// An object that can send an event to an event sink.
pub(super) struct EventSinkSender<T, W> {
    writer: W,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_event: PhantomData<T>,
}

impl<T, W> EventSinkSender<T, W> {
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

impl<T, W: Clone> Clone for EventSinkSender<T, W> {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

/// An object that can send mapped events to an event sink.
pub(super) struct MapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> U,
{
    writer: W,
    map: Arc<C>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_event: PhantomData<T>,
}

impl<T, U, W, C> MapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> U,
{
    pub(super) fn new(map: C, writer: W) -> Self {
        Self {
            writer,
            map: Arc::new(map),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

impl<T, U, W, C> Sender<T, ()> for MapEventSinkSender<T, U, W, C>
where
    T: Send + 'static,
    U: Send + 'static,
    C: Fn(T) -> U + Send + Sync,
    W: EventSinkWriter<U>,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let writer = &mut self.writer;
        let arg = (self.map)(arg);

        RecycledFuture::new(&mut self.fut_storage, async move {
            writer.write(arg);

            Ok(())
        })
    }
}

impl<T, U, W, C> Clone for MapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> U,
    W: Clone,
{
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            map: self.map.clone(),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

/// An object that can filter and send mapped events to an event sink.
pub(super) struct FilterMapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> Option<U>,
{
    writer: W,
    filter_map: Arc<C>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_event: PhantomData<T>,
}

impl<T, U, W, C> FilterMapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> Option<U>,
{
    pub(super) fn new(filter_map: C, writer: W) -> Self {
        Self {
            writer,
            filter_map: Arc::new(filter_map),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

impl<T, U, W, C> Sender<T, ()> for FilterMapEventSinkSender<T, U, W, C>
where
    T: Send + 'static,
    U: Send + 'static,
    C: Fn(T) -> Option<U> + Send + Sync,
    W: EventSinkWriter<U>,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<(), SendError>> {
        let writer = &mut self.writer;

        match (self.filter_map)(arg) {
            Some(arg) => RecycledFuture::new(&mut self.fut_storage, async move {
                writer.write(arg);

                Ok(())
            }),
            None => RecycledFuture::new(&mut self.fut_storage, ready(Ok(()))),
        }
    }
}

impl<T, U, W, C> Clone for FilterMapEventSinkSender<T, U, W, C>
where
    C: Fn(T) -> Option<U>,
    W: Clone,
{
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            filter_map: self.filter_map.clone(),
            fut_storage: None,
            _phantom_event: PhantomData,
        }
    }
}

/// An object that can send requests to a replier port and retrieve responses.
pub(super) struct ReplierSender<M, F, T, R, S>
where
    M: Model,
{
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
    F: Clone,
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

/// An object that can send mapped requests to a replier port and retrieve
/// mapped responses.
pub(super) struct MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: Model,
{
    query_map: Arc<C>,
    reply_map: Arc<D>,
    func: F,
    sender: channel::Sender<M>,
    receiver: multishot::Receiver<Q>,
    fut_storage: Option<RecycleBox<()>>,
    _phantom_query_map: PhantomData<fn(T) -> U>,
    _phantom_reply_map: PhantomData<fn(Q) -> R>,
    _phantom_closure: PhantomData<fn(&mut M, U) -> Q>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, D, F, T, R, U, Q, S> MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: Model,
{
    pub(super) fn new(query_map: C, reply_map: D, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            query_map: Arc::new(query_map),
            reply_map: Arc::new(reply_map),
            func,
            sender,
            receiver: multishot::Receiver::new(),
            fut_storage: None,
            _phantom_query_map: PhantomData,
            _phantom_reply_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, D, F, T, R, U, Q, S> Sender<T, R> for MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: Model,
    C: Fn(T) -> U + Send + Sync,
    D: Fn(Q) -> R + Send + Sync,
    F: for<'a> ReplierFn<'a, M, U, Q, S> + Clone,
    T: Send + 'static,
    R: Send + 'static,
    U: Send + 'static,
    Q: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: T) -> RecycledFuture<'_, Result<R, SendError>> {
        let func = self.func.clone();
        let arg = (self.query_map)(arg);
        let sender = &mut self.sender;
        let reply_receiver = &mut self.receiver;
        let fut_storage = &mut self.fut_storage;
        let reply_map = &*self.reply_map;

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
            reply_receiver
                .recv()
                .await
                .map_err(|_| SendError {})
                .map(reply_map)
        })
    }
}

impl<M, C, D, F, T, R, U, Q, S> Clone for MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: Model,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            query_map: self.query_map.clone(),
            reply_map: self.reply_map.clone(),
            func: self.func.clone(),
            sender: self.sender.clone(),
            receiver: multishot::Receiver::new(),
            fut_storage: None,
            _phantom_query_map: PhantomData,
            _phantom_reply_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
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
