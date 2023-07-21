//! Scheduling functions and types.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel::{ChannelId, Sender};
use crate::model::{InputFn, Model};
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::priority_queue::PriorityQueue;
use crate::util::sync_cell::SyncCellReader;

/// Shorthand for the scheduler queue type.
pub(crate) type SchedulerQueue =
    PriorityQueue<(MonotonicTime, ChannelId), Box<dyn EventFuture<Output = ()> + Send>>;

/// A local scheduler for models.
///
/// A `Scheduler` is a handle to the global scheduler associated to a model
/// instance. It can be used by the model to retrieve the simulation time, to
/// schedule delayed actions on itself or to cancel such actions.
///
/// ### Caveat: self-scheduling `async` methods
///
/// Due to a current rustc issue, `async` methods that schedule themselves will
/// not compile unless an explicit `Send` bound is added to the returned future.
/// This can be done by replacing the `async` signature with a partially
/// desugared signature such as:
///
/// ```ignore
/// fn self_scheduling_method<'a>(
///     &'a mut self,
///     arg: MyEventType,
///     scheduler: &'a Scheduler<Self>
/// ) -> impl Future<Output=()> + Send + 'a {
///     async move {
///         /* implementation */
///     }
/// }
/// ```
///
/// Self-scheduling methods which are not `async` are not affected by this
/// issue.
///
/// # Examples
///
/// A model that sends a greeting after some delay.
///
/// ```
/// use std::time::Duration;
/// use asynchronix::model::{Model, Output}; use asynchronix::time::Scheduler;
///
/// #[derive(Default)]
/// pub struct DelayedGreeter {
///     msg_out: Output<String>
/// }
/// impl DelayedGreeter {
///     // Triggers a greeting on the output port after some delay [input port].
///     pub async fn greet_with_delay(&mut self, delay: Duration, scheduler: &Scheduler<Self>) {
///         let time = scheduler.time();
///         let greeting = format!("Hello, this message was scheduled at:
///         {:?}.", time);
///         
///         if let Err(err) = scheduler.schedule_event_in(delay, Self::send_msg, greeting) {
///             //                                                     ^^^^^^^^ scheduled method
///             // The duration was zero, so greet right away.
///             let greeting = err.0;
///             self.msg_out.send(greeting).await;
///         }
///     }
///
///     // Sends a message to the output [private input port].
///     async fn send_msg(&mut self, msg: String) {
///         self.msg_out.send(msg).await;
///     }
/// }
/// impl Model for DelayedGreeter {}
/// ```

// The self-scheduling caveat seems related to this issue:
// https://github.com/rust-lang/rust/issues/78649
pub struct Scheduler<M: Model> {
    sender: Sender<M>,
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: SyncCellReader<TearableAtomicTime>,
}

impl<M: Model> Scheduler<M> {
    /// Creates a new local scheduler.
    pub(crate) fn new(
        sender: Sender<M>,
        scheduler_queue: Arc<Mutex<SchedulerQueue>>,
        time: SyncCellReader<TearableAtomicTime>,
    ) -> Self {
        Self {
            sender,
            scheduler_queue,
            time,
        }
    }

    /// Returns the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{MonotonicTime, Scheduler};
    ///
    /// fn is_third_millenium<M: Model>(scheduler: &Scheduler<M>) -> bool {
    ///     let time = scheduler.time();
    ///
    ///     time >= MonotonicTime::new(978307200, 0) && time < MonotonicTime::new(32535216000, 0)
    /// }
    /// ```
    pub fn time(&self) -> MonotonicTime {
        self.time.try_read().expect("internal simulation error: could not perform a synchronized read of the simulation time")
    }

    /// Schedules an event at the lapse of the specified duration.
    ///
    /// An error is returned if the specified duration is null.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use std::future::Future;
    /// use asynchronix::model::Model;
    /// use asynchronix::time::Scheduler;
    ///
    /// // A model that logs the value of a counter every second after being
    /// // triggered the first time.
    /// pub struct PeriodicLogger {}
    ///
    /// impl PeriodicLogger {
    ///     // Triggers the logging of a timestamp every second [input port].
    ///     pub fn trigger(&mut self, counter: u64, scheduler: &Scheduler<Self>) {
    ///         println!("counter: {}", counter);
    ///
    ///         // Schedule this method again in 1s with an incremented counter.
    ///         scheduler
    ///             .schedule_event_in(Duration::from_secs(1), Self::trigger, counter + 1)
    ///             .unwrap();
    ///     }
    /// }
    ///
    /// impl Model for PeriodicLogger {}
    /// ```
    pub fn schedule_event_in<F, T, S>(
        &self,
        duration: Duration,
        func: F,
        arg: T,
    ) -> Result<(), ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if duration.is_zero() {
            return Err(ScheduledTimeError(arg));
        }
        let time = self.time() + duration;
        let sender = self.sender.clone();
        schedule_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(())
    }

    /// Schedules an event at the lapse of the specified duration and returns an
    /// event key.
    ///
    /// An error is returned if the specified duration is null.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use std::future::Future;
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{EventKey, Scheduler};
    ///
    /// // A model that logs the value of a counter every second after being
    /// // triggered the first time until logging is stopped.
    /// pub struct PeriodicLogger {
    ///     event_key: Option<EventKey>
    /// }
    ///
    /// impl PeriodicLogger {
    ///     // Triggers the logging of a timestamp every second [input port].
    ///     pub fn trigger(&mut self, counter: u64, scheduler: &Scheduler<Self>) {
    ///         self.stop();
    ///         println!("counter: {}", counter);
    ///
    ///         // Schedule this method again in 1s with an incremented counter.
    ///         let event_key = scheduler
    ///             .schedule_keyed_event_in(Duration::from_secs(1), Self::trigger, counter + 1)
    ///             .unwrap();
    ///         self.event_key = Some(event_key);
    ///     }
    ///
    ///     // Cancels the logging of timestamps.
    ///     pub fn stop(&mut self) {
    ///         self.event_key.take().map(|k| k.cancel_event());
    ///     }
    /// }
    ///
    /// impl Model for PeriodicLogger {}
    /// ```
    pub fn schedule_keyed_event_in<F, T, S>(
        &self,
        duration: Duration,
        func: F,
        arg: T,
    ) -> Result<EventKey, ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if duration.is_zero() {
            return Err(ScheduledTimeError(arg));
        }
        let time = self.time() + duration;
        let sender = self.sender.clone();
        let event_key =
            schedule_keyed_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(event_key)
    }

    /// Schedules an event at a future time.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{MonotonicTime, Scheduler};
    ///
    /// // An alarm clock.
    /// pub struct AlarmClock {
    ///     msg: String
    /// }
    ///
    /// impl AlarmClock {
    ///     // Creates a new alarm clock.
    ///     pub fn new(msg: String) -> Self {
    ///         Self { msg }
    ///     }
    ///
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
    ///         if scheduler.schedule_event_at(setting, Self::ring, ()).is_err() {
    ///             println!("The alarm clock can only be set for a future time");
    ///         }
    ///     }
    ///
    ///     // Rings the alarm [private input port].
    ///     fn ring(&mut self) {
    ///         println!("{}", self.msg);
    ///     }
    /// }
    ///
    /// impl Model for AlarmClock {}
    /// ```
    pub fn schedule_event_at<F, T, S>(
        &self,
        time: MonotonicTime,
        func: F,
        arg: T,
    ) -> Result<(), ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if self.time() >= time {
            return Err(ScheduledTimeError(arg));
        }
        let sender = self.sender.clone();
        schedule_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(())
    }

    /// Schedules an event at a future time and returns an event key.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{EventKey, MonotonicTime, Scheduler};
    ///
    /// // An alarm clock that can be cancelled.
    /// pub struct AlarmClock {
    ///     msg: String,
    ///     event_key: Option<EventKey>,
    /// }
    ///
    /// impl AlarmClock {
    ///     // Creates a new alarm clock.
    ///     pub fn new(msg: String) -> Self {
    ///         Self {
    ///             msg,
    ///             event_key: None
    ///         }
    ///     }
    ///
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
    ///         self.cancel();
    ///         match scheduler.schedule_keyed_event_at(setting, Self::ring, ()) {
    ///             Ok(event_key) => self.event_key = Some(event_key),
    ///             Err(_) => println!("The alarm clock can only be set for a future time"),
    ///         };
    ///     }
    ///
    ///     // Cancels the current alarm, if any.
    ///     pub fn cancel(&mut self) {
    ///         self.event_key.take().map(|k| k.cancel_event());
    ///     }
    ///
    ///     // Rings the alarm [private input port].
    ///     fn ring(&mut self) {
    ///         println!("{}", self.msg);
    ///     }
    /// }
    ///
    /// impl Model for AlarmClock {}
    /// ```
    pub fn schedule_keyed_event_at<F, T, S>(
        &self,
        time: MonotonicTime,
        func: F,
        arg: T,
    ) -> Result<EventKey, ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if self.time() >= time {
            return Err(ScheduledTimeError(arg));
        }
        let sender = self.sender.clone();
        let event_key =
            schedule_keyed_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(event_key)
    }
}

impl<M: Model> fmt::Debug for Scheduler<M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

/// Handle to a scheduled event.
///
/// An `EventKey` can be used to cancel a future event.
#[derive(Clone, Debug)]
pub struct EventKey {
    state: Arc<AtomicUsize>,
}

impl EventKey {
    const IS_PENDING: usize = 0;
    const IS_CANCELLED: usize = 1;
    const IS_PROCESSED: usize = 2;

    /// Creates a key for a pending event.
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AtomicUsize::new(Self::IS_PENDING)),
        }
    }

    /// Checks whether the event was cancelled.
    pub(crate) fn event_is_cancelled(&self) -> bool {
        self.state.load(Ordering::Relaxed) == Self::IS_CANCELLED
    }

    /// Marks the event as processed.
    ///
    /// If the event cannot be processed because it was cancelled, `false` is
    /// returned.
    pub(crate) fn process_event(self) -> bool {
        match self.state.compare_exchange(
            Self::IS_PENDING,
            Self::IS_PROCESSED,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => true,
            Err(s) => s == Self::IS_PROCESSED,
        }
    }

    /// Cancels the associated event if possible.
    ///
    /// If the event cannot be cancelled because it was already processed,
    /// `false` is returned.
    pub fn cancel_event(self) -> bool {
        match self.state.compare_exchange(
            Self::IS_PENDING,
            Self::IS_CANCELLED,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => true,
            Err(s) => s == Self::IS_CANCELLED,
        }
    }
}

/// Error returned when the scheduled time does not lie in the future of the
/// current simulation time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct ScheduledTimeError<T>(pub T);

impl<T> fmt::Display for ScheduledTimeError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "the scheduled time should be in the future of the current simulation time"
        )
    }
}

impl<T: fmt::Debug> Error for ScheduledTimeError<T> {}

/// Schedules an event at a future time.
///
/// This method does not check whether the specified time lies in the future
/// of the current simulation time.
pub(crate) fn schedule_event_at_unchecked<M, F, T, S>(
    time: MonotonicTime,
    func: F,
    arg: T,
    sender: Sender<M>,
    scheduler_queue: &Mutex<SchedulerQueue>,
) where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + Clone + 'static,
{
    let channel_id = sender.channel_id();

    let fut = async move {
        let _ = sender
            .send(
                move |model: &mut M,
                      scheduler,
                      recycle_box: RecycleBox<()>|
                      -> RecycleBox<dyn Future<Output = ()> + Send + '_> {
                    let fut = func.call(model, arg, scheduler);

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                },
            )
            .await;
    };
    let fut = Box::new(UnkeyedEventFuture::new(fut));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), fut);
}

/// Schedules an event at a future time, returning an event key.
///
/// This method does not check whether the specified time lies in the future
/// of the current simulation time.
pub(crate) fn schedule_keyed_event_at_unchecked<M, F, T, S>(
    time: MonotonicTime,
    func: F,
    arg: T,
    sender: Sender<M>,
    scheduler_queue: &Mutex<SchedulerQueue>,
) -> EventKey
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + Clone + 'static,
{
    let channel_id = sender.channel_id();

    let event_key = EventKey::new();
    let local_event_key = event_key.clone();

    let fut = async move {
        let _ = sender
            .send(
                move |model: &mut M,
                      scheduler,
                      recycle_box: RecycleBox<()>|
                      -> RecycleBox<dyn Future<Output = ()> + Send + '_> {
                    let fut = async move {
                        if local_event_key.process_event() {
                            func.call(model, arg, scheduler).await;
                        }
                    };

                    coerce_box!(RecycleBox::recycle(recycle_box, fut))
                },
            )
            .await;
    };

    // Implementation note: we end up with two atomic references to the event
    // key stored inside the event future: one was moved above to the future
    // itself and the other one is created below via cloning and stored
    // separately in the `KeyedEventFuture`. This is not ideal as we could
    // theoretically spare on atomic reference counting by storing a single
    // reference, but this would likely require some tricky `unsafe`, not least
    // because the inner future sent to the mailbox outlives the
    // `KeyedEventFuture`.
    let fut = Box::new(KeyedEventFuture::new(fut, event_key.clone()));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), fut);

    event_key
}

/// The future of an event which scheduling may be cancelled by the user.
pub(crate) trait EventFuture: Future {
    /// Whether the scheduling of this event was cancelled.
    fn is_cancelled(&self) -> bool;
}

pin_project! {
    /// Future associated to a regular event that cannot be cancelled.
    pub(crate) struct UnkeyedEventFuture<F> {
        #[pin]
        fut: F,
    }
}

impl<F> UnkeyedEventFuture<F> {
    /// Creates a new `EventFuture`.
    pub(crate) fn new(fut: F) -> Self {
        Self { fut }
    }
}

impl<F> Future for UnkeyedEventFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    #[inline(always)]
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

impl<F> EventFuture for UnkeyedEventFuture<F>
where
    F: Future,
{
    fn is_cancelled(&self) -> bool {
        false
    }
}

pin_project! {
    /// Future associated to a keyed event that can be cancelled.
    pub(crate) struct KeyedEventFuture<F> {
        event_key: EventKey,
        #[pin]
        fut: F,
    }
}

impl<F> KeyedEventFuture<F> {
    /// Creates a new `EventFuture`.
    pub(crate) fn new(fut: F, event_key: EventKey) -> Self {
        Self { event_key, fut }
    }
}

impl<F> Future for KeyedEventFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    #[inline(always)]
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

impl<F> EventFuture for KeyedEventFuture<F>
where
    F: Future,
{
    fn is_cancelled(&self) -> bool {
        self.event_key.event_is_cancelled()
    }
}
