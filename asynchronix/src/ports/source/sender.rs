use std::error::Error;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

use futures_channel::oneshot;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel;
use crate::model::Model;
use crate::ports::{InputFn, ReplierFn};

pub(super) type SenderFuture<R> = Pin<Box<dyn Future<Output = Result<R, SendError>> + Send>>;

/// An event or query sender abstracting over the target model and input method.
pub(super) trait Sender<T, R>: Send {
    /// Asynchronously send the event or request.
    fn send(&mut self, arg: T) -> SenderFuture<R>;
}

/// An object that can send events to an input port.
pub(super) struct InputSender<M: 'static, F, T, S> {
    func: F,
    sender: channel::Sender<M>,
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
    fn send(&mut self, arg: T) -> SenderFuture<()> {
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
        })
    }
}

/// An object that can send a request to a replier port and retrieve a response.
pub(super) struct ReplierSender<M: 'static, F, T, R, S> {
    func: F,
    sender: channel::Sender<M>,
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
    fn send(&mut self, arg: T) -> SenderFuture<R> {
        let func = self.func.clone();
        let sender = self.sender.clone();
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

            reply_receiver.await.map_err(|_| SendError {})
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
