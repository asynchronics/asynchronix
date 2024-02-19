//! Scheduling functions and types.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel::Sender;
use crate::executor::Executor;
use crate::model::{InputFn, Model};
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::priority_queue::PriorityQueue;
use crate::util::sync_cell::SyncCellReader;

/// Shorthand for the scheduler queue type.

// Why use both time and channel ID as the key? The short answer is that this
// ensures that events targeting the same channel are sent in the order they
// were scheduled. More precisely, this ensures that events targeting the same
// channel are ordered contiguously in the priority queue, which in turns allows
// the event loop to easily aggregate such events into single futures and thus
// control their relative order of execution.
pub(crate) type SchedulerQueue =
    PriorityQueue<(MonotonicTime, NonZeroUsize), Box<dyn ScheduledEvent>>;

/// Trait abstracting over time-absolute and time-relative deadlines.
///
/// This trait is implemented by [`std::time::Duration`] and
/// [`MonotonicTime`].
pub trait Deadline {
    /// Make this deadline into an absolute timestamp, using the provided
    /// current time as a reference.
    fn into_time(self, now: MonotonicTime) -> MonotonicTime;
}

impl Deadline for Duration {
    #[inline(always)]
    fn into_time(self, now: MonotonicTime) -> MonotonicTime {
        now + self
    }
}

impl Deadline for MonotonicTime {
    #[inline(always)]
    fn into_time(self, _: MonotonicTime) -> MonotonicTime {
        self
    }
}

/// A local scheduler for models.
///
/// A `Scheduler` is a handle to the global scheduler associated to a model
/// instance. It can be used by the model to retrieve the simulation time or
/// schedule delayed actions on itself.
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
///     msg_out: Output<String>,
/// }
///
/// impl DelayedGreeter {
///     // Triggers a greeting on the output port after some delay [input port].
///     pub async fn greet_with_delay(&mut self, delay: Duration, scheduler: &Scheduler<Self>) {
///         let time = scheduler.time();
///         let greeting = format!("Hello, this message was scheduled at: {:?}.", time);
///         
///         if delay.is_zero() {
///             self.msg_out.send(greeting).await;
///         } else {
///             scheduler.schedule_event(delay, Self::send_msg, greeting).unwrap();
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

    /// Schedules an event at a future time.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use asynchronix::model::Model;
    /// use asynchronix::time::Scheduler;
    ///
    /// // A timer.
    /// pub struct Timer {}
    ///
    /// impl Timer {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: Duration, scheduler: &Scheduler<Self>) {
    ///         if scheduler.schedule_event(setting, Self::ring, ()).is_err() {
    ///             println!("The alarm clock can only be set for a future time");
    ///         }
    ///     }
    ///
    ///     // Rings [private input port].
    ///     fn ring(&mut self) {
    ///         println!("Brringggg");
    ///     }
    /// }
    ///
    /// impl Model for Timer {}
    /// ```
    pub fn schedule_event<F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
    ) -> Result<(), SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        let sender = self.sender.clone();
        schedule_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(())
    }

    /// Schedules a cancellable event at a future time and returns an event key.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{EventKey, MonotonicTime, Scheduler};
    ///
    /// // An alarm clock that can be cancelled.
    /// #[derive(Default)]
    /// pub struct CancellableAlarmClock {
    ///     event_key: Option<EventKey>,
    /// }
    ///
    /// impl CancellableAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
    ///         self.cancel();
    ///         match scheduler.schedule_keyed_event(setting, Self::ring, ()) {
    ///             Ok(event_key) => self.event_key = Some(event_key),
    ///             Err(_) => println!("The alarm clock can only be set for a future time"),
    ///         };
    ///     }
    ///
    ///     // Cancels the current alarm, if any [input port].
    ///     pub fn cancel(&mut self) {
    ///         self.event_key.take().map(|k| k.cancel());
    ///     }
    ///
    ///     // Rings the alarm [private input port].
    ///     fn ring(&mut self) {
    ///         println!("Brringggg!");
    ///     }
    /// }
    ///
    /// impl Model for CancellableAlarmClock {}
    /// ```
    pub fn schedule_keyed_event<F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
    ) -> Result<EventKey, SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        let sender = self.sender.clone();
        let event_key =
            schedule_keyed_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(event_key)
    }

    /// Schedules a periodically recurring event at a future time.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time or if the specified period is null.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{MonotonicTime, Scheduler};
    ///
    /// // An alarm clock beeping at 1Hz.
    /// pub struct BeepingAlarmClock {}
    ///
    /// impl BeepingAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
    ///         if scheduler.schedule_periodic_event(
    ///             setting,
    ///             Duration::from_secs(1), // 1Hz = 1/1s
    ///             Self::beep,
    ///             ()
    ///         ).is_err() {
    ///             println!("The alarm clock can only be set for a future time");
    ///         }
    ///     }
    ///
    ///     // Emits a single beep [private input port].
    ///     fn beep(&mut self) {
    ///         println!("Beep!");
    ///     }
    /// }
    ///
    /// impl Model for BeepingAlarmClock {}
    /// ```
    pub fn schedule_periodic_event<F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
    ) -> Result<(), SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let sender = self.sender.clone();
        schedule_periodic_event_at_unchecked(
            time,
            period,
            func,
            arg,
            sender,
            &self.scheduler_queue,
        );

        Ok(())
    }

    /// Schedules a cancellable, periodically recurring event at a future time
    /// and returns an event key.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time or if the specified period is null.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use asynchronix::model::Model;
    /// use asynchronix::time::{EventKey, MonotonicTime, Scheduler};
    ///
    /// // An alarm clock beeping at 1Hz that can be cancelled before it sets off, or
    /// // stopped after it sets off.
    /// #[derive(Default)]
    /// pub struct CancellableBeepingAlarmClock {
    ///     event_key: Option<EventKey>,
    /// }
    ///
    /// impl CancellableBeepingAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
    ///         self.cancel();
    ///         match scheduler.schedule_keyed_periodic_event(
    ///             setting,
    ///             Duration::from_secs(1), // 1Hz = 1/1s
    ///             Self::beep,
    ///             ()
    ///         ) {
    ///             Ok(event_key) => self.event_key = Some(event_key),
    ///             Err(_) => println!("The alarm clock can only be set for a future time"),
    ///         };
    ///     }
    ///
    ///     // Cancels or stops the alarm [input port].
    ///     pub fn cancel(&mut self) {
    ///         self.event_key.take().map(|k| k.cancel());
    ///     }
    ///
    ///     // Emits a single beep [private input port].
    ///     fn beep(&mut self) {
    ///         println!("Beep!");
    ///     }
    /// }
    ///
    /// impl Model for CancellableBeepingAlarmClock {}
    /// ```
    pub fn schedule_keyed_periodic_event<F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
    ) -> Result<EventKey, SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let sender = self.sender.clone();
        let event_key = schedule_periodic_keyed_event_at_unchecked(
            time,
            period,
            func,
            arg,
            sender,
            &self.scheduler_queue,
        );

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
#[must_use = "prefer unkeyed scheduling methods if the event is never cancelled"]
pub struct EventKey {
    is_cancelled: Arc<AtomicBool>,
}

impl EventKey {
    /// Creates a key for a pending event.
    pub(crate) fn new() -> Self {
        Self {
            is_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Checks whether the event was cancelled.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Relaxed)
    }

    /// Cancels the associated event.
    pub fn cancel(self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
    }
}

/// Error returned when the scheduled time or the repetition period are invalid.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SchedulingError {
    /// The scheduled time does not lie in the future of the current simulation
    /// time.
    InvalidScheduledTime,
    /// The repetition period is zero.
    NullRepetitionPeriod,
}

impl fmt::Display for SchedulingError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScheduledTime => write!(
                fmt,
                "the scheduled time should be in the future of the current simulation time"
            ),
            Self::NullRepetitionPeriod => write!(fmt, "the repetition period cannot be zero"),
        }
    }
}

impl Error for SchedulingError {}

/// Schedules an event at a future time.
///
/// This function does not check whether the specified time lies in the future
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
    S: Send + 'static,
{
    let channel_id = sender.channel_id();

    let event_dispatcher = Box::new(EventDispatcher::new(dispatch_event(func, arg, sender)));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), event_dispatcher);
}

/// Schedules an event at a future time, returning an event key.
///
/// This function does not check whether the specified time lies in the future
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
    S: Send + 'static,
{
    let event_key = EventKey::new();
    let channel_id = sender.channel_id();
    let event_dispatcher = Box::new(KeyedEventDispatcher::new(
        |ek| dispatch_keyed_event(ek, func, arg, sender),
        event_key.clone(),
    ));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), event_dispatcher);

    event_key
}

/// Schedules a periodic event at a future time.
///
/// This function does not check whether the specified time lies in the future
/// of the current simulation time.
pub(crate) fn schedule_periodic_event_at_unchecked<M, F, T, S>(
    time: MonotonicTime,
    period: Duration,
    func: F,
    arg: T,
    sender: Sender<M>,
    scheduler_queue: &Mutex<SchedulerQueue>,
) where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Send + Clone + 'static,
    S: Send + 'static,
{
    let channel_id = sender.channel_id();

    let event_dispatcher = Box::new(PeriodicEventDispatcher::new(
        || dispatch_event(func, arg, sender),
        period,
    ));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), event_dispatcher);
}

/// Schedules an event at a future time, returning an event key.
///
/// This function does not check whether the specified time lies in the future
/// of the current simulation time.
pub(crate) fn schedule_periodic_keyed_event_at_unchecked<M, F, T, S>(
    time: MonotonicTime,
    period: Duration,
    func: F,
    arg: T,
    sender: Sender<M>,
    scheduler_queue: &Mutex<SchedulerQueue>,
) -> EventKey
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S> + Clone,
    T: Send + Clone + 'static,
    S: Send + 'static,
{
    let event_key = EventKey::new();
    let channel_id = sender.channel_id();
    let event_dispatcher = Box::new(PeriodicKeyedEventDispatcher::new(
        |ek| dispatch_keyed_event(ek, func, arg, sender),
        period,
        event_key.clone(),
    ));

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    scheduler_queue.insert((time, channel_id), event_dispatcher);

    event_key
}

/// Trait for objects that can be converted to a future dispatching a scheduled
/// event.
pub(crate) trait ScheduledEvent: Send {
    /// Reports whether the associated event was cancelled.
    fn is_cancelled(&self) -> bool;

    /// Returns a boxed clone of this event and the repetition period if this is
    /// a periodic even, otherwise returns `None`.
    fn next(&self) -> Option<(Box<dyn ScheduledEvent>, Duration)>;

    /// Returns a boxed future dispatching the associated event.
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Spawns the future that dispatches the associated event onto the provided
    /// executor.
    ///
    /// This method is typically more efficient that spawning the boxed future
    /// from `into_future` since it can directly spawn the unboxed future.
    fn spawn_and_forget(self: Box<Self>, executor: &Executor);
}

pin_project! {
    /// An object that can be converted to a future dispatching a
    /// non-cancellable event.
    ///
    /// Note that this particular event dispatcher is in fact already a future:
    /// since the future cannot be cancelled and the dispatcher does not need to
    /// be cloned, there is no need to defer the construction of the future.
    /// This makes `into_future` a trivial cast, which saves a boxing operation.
    pub(crate) struct EventDispatcher<F> {
        #[pin]
        fut: F,
    }
}

impl<F> EventDispatcher<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `EventDispatcher`.
    pub(crate) fn new(fut: F) -> Self {
        EventDispatcher { fut }
    }
}

impl<F> Future for EventDispatcher<F>
where
    F: Future,
{
    type Output = F::Output;

    #[inline(always)]
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

impl<F> ScheduledEvent for EventDispatcher<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        false
    }
    fn next(&self) -> Option<(Box<dyn ScheduledEvent>, Duration)> {
        None
    }
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // No need for boxing, type coercion is enough here.
        Box::into_pin(self)
    }
    fn spawn_and_forget(self: Box<Self>, executor: &Executor) {
        executor.spawn_and_forget(*self);
    }
}

/// An object that can be converted to a future dispatching a non-cancellable,
/// periodic event.
pub(crate) struct PeriodicEventDispatcher<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A clonable generator for the dispatching future.
    gen: G,
    /// The event repetition period.
    period: Duration,
}

impl<G, F> PeriodicEventDispatcher<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `PeriodicEventDispatcher`.
    fn new(gen: G, period: Duration) -> Self {
        Self { gen, period }
    }
}

impl<G, F> ScheduledEvent for PeriodicEventDispatcher<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        false
    }
    fn next(&self) -> Option<(Box<dyn ScheduledEvent>, Duration)> {
        let event = Box::new(Self::new(self.gen.clone(), self.period));

        Some((event, self.period))
    }
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self.gen)())
    }
    fn spawn_and_forget(self: Box<Self>, executor: &Executor) {
        executor.spawn_and_forget((self.gen)());
    }
}

/// An object that can be converted to a future dispatching a cancellable event.
pub(crate) struct KeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A generator for the dispatching future.
    gen: G,
    /// The event cancellation key.
    event_key: EventKey,
}

impl<G, F> KeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `KeyedEventDispatcher`.
    fn new(gen: G, event_key: EventKey) -> Self {
        Self { gen, event_key }
    }
}

impl<G, F> ScheduledEvent for KeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        self.event_key.is_cancelled()
    }
    fn next(&self) -> Option<(Box<dyn ScheduledEvent>, Duration)> {
        None
    }
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self.gen)(self.event_key))
    }
    fn spawn_and_forget(self: Box<Self>, executor: &Executor) {
        executor.spawn_and_forget((self.gen)(self.event_key));
    }
}

/// An object that can be converted to a future dispatching a periodic,
/// cancellable event.
pub(crate) struct PeriodicKeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A clonable generator for the dispatching future.
    gen: G,
    /// The repetition period.
    period: Duration,
    /// The event cancellation key.
    event_key: EventKey,
}

impl<G, F> PeriodicKeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `KeyedEventDispatcher`.
    fn new(gen: G, period: Duration, event_key: EventKey) -> Self {
        Self {
            gen,
            period,
            event_key,
        }
    }
}

impl<G, F> ScheduledEvent for PeriodicKeyedEventDispatcher<G, F>
where
    G: (FnOnce(EventKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        self.event_key.is_cancelled()
    }
    fn next(&self) -> Option<(Box<dyn ScheduledEvent>, Duration)> {
        let event = Box::new(Self::new(
            self.gen.clone(),
            self.period,
            self.event_key.clone(),
        ));

        Some((event, self.period))
    }
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self.gen)(self.event_key))
    }
    fn spawn_and_forget(self: Box<Self>, executor: &Executor) {
        executor.spawn_and_forget((self.gen)(self.event_key));
    }
}

/// Asynchronously dispatches a non-cancellable event to a model input.
pub(crate) async fn dispatch_event<M, F, T, S>(func: F, arg: T, sender: Sender<M>)
where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + 'static,
{
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
}

/// Asynchronously dispatches a cancellable event to a model input.
pub(crate) async fn dispatch_keyed_event<M, F, T, S>(
    event_key: EventKey,
    func: F,
    arg: T,
    sender: Sender<M>,
) where
    M: Model,
    F: for<'a> InputFn<'a, M, T, S>,
    T: Send + Clone + 'static,
{
    let _ = sender
        .send(
            move |model: &mut M,
                  scheduler,
                  recycle_box: RecycleBox<()>|
                  -> RecycleBox<dyn Future<Output = ()> + Send + '_> {
                let fut = async move {
                    // Only perform the call if the event wasn't cancelled.
                    if !event_key.is_cancelled() {
                        func.call(model, arg, scheduler).await;
                    }
                };

                coerce_box!(RecycleBox::recycle(recycle_box, fut))
            },
        )
        .await;
}
