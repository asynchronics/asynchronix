use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use futures_channel::oneshot;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel;
use crate::model::Model;
use crate::ports::{InputFn, ReplierFn};

pub(super) type SenderFuture<R> = Pin<Box<dyn Future<Output = Result<R, SendError>> + Send>>;

/// An event or query sender abstracting over the target model and input method.
pub(super) trait Sender<T, R>: Send {
    /// Asynchronously sends a message using a reference to the message.
    fn send(&mut self, arg: &T) -> Option<SenderFuture<R>>;

    /// Asynchronously sends an owned message.
    fn send_owned(&mut self, arg: T) -> Option<SenderFuture<R>> {
        self.send(&arg)
    }
}

/// An object that can send events to an input port.
pub(super) struct InputSender<M, F, T, S>
where
    M: 'static,
{
    func: F,
    sender: channel::Sender<M>,
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
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, F, T, S> Sender<T, ()> for InputSender<M, F, T, S>
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Clone + Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<()>> {
        self.send_owned(arg.clone())
    }

    fn send_owned(&mut self, arg: T) -> Option<SenderFuture<()>> {
        let func = self.func.clone();
        let sender = self.sender.clone();

        Some(Box::pin(async move {
            sender
                .send(move |model, scheduler, recycle_box| {
                    let fut = func.call(model, arg, scheduler);

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                })
                .await
                .map_err(|_| SendError {})
        }))
    }
}

/// An object that can send mapped events to an input port.
pub(super) struct MapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    map: C,
    func: F,
    sender: channel::Sender<M>,
    _phantom_map: PhantomData<fn(T) -> U>,
    _phantom_closure: PhantomData<fn(&mut M, T)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, F, T, U, S> MapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    pub(super) fn new(map: C, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            map,
            func,
            sender,
            _phantom_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, F, T, U, S> Sender<T, ()> for MapInputSender<M, C, F, T, U, S>
where
    M: Model,
    C: Fn(&T) -> U + Send,
    F: for<'a> InputFn<'a, M, U, S> + Clone,
    T: Send + 'static,
    U: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<()>> {
        let func = self.func.clone();
        let arg = (self.map)(arg);
        let sender = self.sender.clone();

        Some(Box::pin(async move {
            sender
                .send(move |model, scheduler, recycle_box| {
                    let fut = func.call(model, arg, scheduler);

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                })
                .await
                .map_err(|_| SendError {})
        }))
    }
}

/// An object that can filter and send mapped events to an input port.
pub(super) struct FilterMapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    filter_map: C,
    func: F,
    sender: channel::Sender<M>,
    _phantom_map: PhantomData<fn(T) -> U>,
    _phantom_closure: PhantomData<fn(&mut M, T)>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, F, T, U, S> FilterMapInputSender<M, C, F, T, U, S>
where
    M: 'static,
{
    pub(super) fn new(filter_map: C, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            filter_map,
            func,
            sender,
            _phantom_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, F, T, U, S> Sender<T, ()> for FilterMapInputSender<M, C, F, T, U, S>
where
    M: Model,
    C: Fn(&T) -> Option<U> + Send,
    F: for<'a> InputFn<'a, M, U, S> + Clone,
    T: Send + 'static,
    U: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<()>> {
        (self.filter_map)(arg).map(|arg| {
            let func = self.func.clone();
            let sender = self.sender.clone();

            Box::pin(async move {
                sender
                    .send(move |model, scheduler, recycle_box| {
                        let fut = func.call(model, arg, scheduler);

                        coerce_box!(RecycleBox::recycle(recycle_box, fut))
                    })
                    .await
                    .map_err(|_| SendError {})
            }) as SenderFuture<()>
        })
    }
}

/// An object that can send a request to a replier port and retrieve a response.
pub(super) struct ReplierSender<M, F, T, R, S>
where
    M: 'static,
{
    func: F,
    sender: channel::Sender<M>,
    _phantom_closure: PhantomData<fn(&mut M, T) -> R>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, F, T, R, S> ReplierSender<M, F, T, R, S>
where
    M: 'static,
{
    pub(super) fn new(func: F, sender: channel::Sender<M>) -> Self {
        Self {
            func,
            sender,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, F, T, R, S> Sender<T, R> for ReplierSender<M, F, T, R, S>
where
    M: Model,
    F: for<'a> ReplierFn<'a, M, T, R, S> + Clone,
    T: Clone + Send + 'static,
    R: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<R>> {
        self.send_owned(arg.clone())
    }

    fn send_owned(&mut self, arg: T) -> Option<SenderFuture<R>> {
        let func = self.func.clone();
        let sender = self.sender.clone();
        let (reply_sender, reply_receiver) = oneshot::channel();

        Some(Box::pin(async move {
            sender
                .send(move |model, scheduler, recycle_box| {
                    let fut = async move {
                        let reply = func.call(model, arg, scheduler).await;
                        let _ = reply_sender.send(reply);
                    };

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                })
                .await
                .map_err(|_| SendError {})?;

            reply_receiver.await.map_err(|_| SendError {})
        }))
    }
}

/// An object that can send a mapped request to a replier port and retrieve a
/// mapped response.
pub(super) struct MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: 'static,
{
    query_map: C,
    reply_map: Arc<D>,
    func: F,
    sender: channel::Sender<M>,
    _phantom_query_map: PhantomData<fn(T) -> U>,
    _phantom_reply_map: PhantomData<fn(Q) -> R>,
    _phantom_closure: PhantomData<fn(&mut M, U) -> Q>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, D, F, T, R, U, Q, S> MapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: 'static,
{
    pub(super) fn new(query_map: C, reply_map: D, func: F, sender: channel::Sender<M>) -> Self {
        Self {
            query_map,
            reply_map: Arc::new(reply_map),
            func,
            sender,
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
    C: Fn(&T) -> U + Send,
    D: Fn(Q) -> R + Send + Sync + 'static,
    F: for<'a> ReplierFn<'a, M, U, Q, S> + Clone,
    T: Send + 'static,
    R: Send + 'static,
    U: Send + 'static,
    Q: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<R>> {
        let func = self.func.clone();
        let arg = (self.query_map)(arg);
        let sender = self.sender.clone();
        let reply_map = self.reply_map.clone();
        let (reply_sender, reply_receiver) = oneshot::channel();

        Some(Box::pin(async move {
            sender
                .send(move |model, scheduler, recycle_box| {
                    let fut = async move {
                        let reply = func.call(model, arg, scheduler).await;
                        let _ = reply_sender.send(reply);
                    };

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                })
                .await
                .map_err(|_| SendError {})?;

            reply_receiver
                .await
                .map_err(|_| SendError {})
                .map(&*reply_map)
        }))
    }
}

/// An object that can filter and send a mapped request to a replier port and
/// retrieve a mapped response.
pub(super) struct FilterMapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: 'static,
{
    query_filter_map: C,
    reply_map: Arc<D>,
    func: F,
    sender: channel::Sender<M>,
    _phantom_query_map: PhantomData<fn(T) -> Option<U>>,
    _phantom_reply_map: PhantomData<fn(Q) -> R>,
    _phantom_closure: PhantomData<fn(&mut M, U) -> Q>,
    _phantom_closure_marker: PhantomData<S>,
}

impl<M, C, D, F, T, R, U, Q, S> FilterMapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: 'static,
{
    pub(super) fn new(
        query_filter_map: C,
        reply_map: D,
        func: F,
        sender: channel::Sender<M>,
    ) -> Self {
        Self {
            query_filter_map,
            reply_map: Arc::new(reply_map),
            func,
            sender,
            _phantom_query_map: PhantomData,
            _phantom_reply_map: PhantomData,
            _phantom_closure: PhantomData,
            _phantom_closure_marker: PhantomData,
        }
    }
}

impl<M, C, D, F, T, R, U, Q, S> Sender<T, R> for FilterMapReplierSender<M, C, D, F, T, R, U, Q, S>
where
    M: Model,
    C: Fn(&T) -> Option<U> + Send,
    D: Fn(Q) -> R + Send + Sync + 'static,
    F: for<'a> ReplierFn<'a, M, U, Q, S> + Clone,
    T: Send + 'static,
    R: Send + 'static,
    U: Send + 'static,
    Q: Send + 'static,
    S: Send,
{
    fn send(&mut self, arg: &T) -> Option<SenderFuture<R>> {
        (self.query_filter_map)(arg).map(|arg| {
            let func = self.func.clone();
            let sender = self.sender.clone();
            let reply_map = self.reply_map.clone();
            let (reply_sender, reply_receiver) = oneshot::channel();

            Box::pin(async move {
                sender
                    .send(move |model, scheduler, recycle_box| {
                        let fut = async move {
                            let reply = func.call(model, arg, scheduler).await;
                            let _ = reply_sender.send(reply);
                        };

                        coerce_box!(RecycleBox::recycle(recycle_box, fut))
                    })
                    .await
                    .map_err(|_| SendError {})?;

                reply_receiver
                    .await
                    .map_err(|_| SendError {})
                    .map(&*reply_map)
            }) as SenderFuture<R>
        })
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
