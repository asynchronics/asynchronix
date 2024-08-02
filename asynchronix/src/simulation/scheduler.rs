//! Scheduling functions and types.

use std::error::Error;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, ptr};

use pin_project_lite::pin_project;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel::Sender;
use crate::executor::Executor;
use crate::model::Model;
use crate::ports::InputFn;
use crate::simulation::Address;
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::priority_queue::PriorityQueue;
use crate::util::sync_cell::SyncCellReader;

/// Scheduler.
#[derive(Clone)]
pub struct Scheduler {
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: SyncCellReader<TearableAtomicTime>,
}

impl Scheduler {
    pub(crate) fn new(
        scheduler_queue: Arc<Mutex<SchedulerQueue>>,
        time: SyncCellReader<TearableAtomicTime>,
    ) -> Self {
        Self {
            scheduler_queue,
            time,
        }
    }

    /// Returns the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::simulation::Scheduler;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// fn is_third_millenium(scheduler: &Scheduler) -> bool {
    ///     let time = scheduler.time();
    ///     time >= MonotonicTime::new(978307200, 0).unwrap()
    ///         && time < MonotonicTime::new(32535216000, 0).unwrap()
    /// }
    /// ```
    pub fn time(&self) -> MonotonicTime {
        self.time.try_read().expect("internal simulation error: could not perform a synchronized read of the simulation time")
    }

    /// Schedules an action at a future time.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time.
    ///
    /// If multiple actions send events at the same simulation time to the same
    /// model, these events are guaranteed to be processed according to the
    /// scheduling order of the actions.
    pub fn schedule(&self, deadline: impl Deadline, action: Action) -> Result<(), SchedulingError> {
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();

        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        // The channel ID is set to the same value for all actions. This
        // ensures that the relative scheduling order of all source events is
        // preserved, which is important if some of them target the same models.
        // The value 0 was chosen as it prevents collisions with channel IDs as
        // the latter are always non-zero.
        scheduler_queue.insert((time, 0), action);

        Ok(())
    }

    /// Schedules an event at a future time.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time.
    ///
    /// Events scheduled for the same time and targeting the same model are
    /// guaranteed to be processed according to the scheduling order.
    ///
    /// See also: [`LocalScheduler::schedule_event`](LocalScheduler::schedule_event).
    pub fn schedule_event<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<(), SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        let sender = address.into().0;
        let channel_id = sender.channel_id();
        let action = Action::new(OnceAction::new(process_event(func, arg, sender)));

        scheduler_queue.insert((time, channel_id), action);

        Ok(())
    }

    /// Schedules a cancellable event at a future time and returns an event key.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time.
    ///
    /// Events scheduled for the same time and targeting the same model are
    /// guaranteed to be processed according to the scheduling order.
    ///
    /// See also: [`LocalScheduler::schedule_keyed_event`](LocalScheduler::schedule_keyed_event).
    pub fn schedule_keyed_event<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<ActionKey, SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        let event_key = ActionKey::new();
        let sender = address.into().0;
        let channel_id = sender.channel_id();
        let action = Action::new(KeyedOnceAction::new(
            |ek| send_keyed_event(ek, func, arg, sender),
            event_key.clone(),
        ));

        scheduler_queue.insert((time, channel_id), action);

        Ok(event_key)
    }

    /// Schedules a periodically recurring event at a future time.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time or if the specified period is null.
    ///
    /// Events scheduled for the same time and targeting the same model are
    /// guaranteed to be processed according to the scheduling order.
    ///
    /// See also: [`LocalScheduler::schedule_periodic_event`](LocalScheduler::schedule_periodic_event).
    pub fn schedule_periodic_event<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<(), SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let sender = address.into().0;
        let channel_id = sender.channel_id();

        let action = Action::new(PeriodicAction::new(
            || process_event(func, arg, sender),
            period,
        ));

        scheduler_queue.insert((time, channel_id), action);

        Ok(())
    }

    /// Schedules a cancellable, periodically recurring event at a future time
    /// and returns an event key.
    ///
    /// An error is returned if the specified time is not in the future of the
    /// current simulation time or if the specified period is null.
    ///
    /// Events scheduled for the same time and targeting the same model are
    /// guaranteed to be processed according to the scheduling order.
    ///
    /// See also: [`LocalScheduler::schedule_keyed_periodic_event`](LocalScheduler::schedule_keyed_periodic_event).
    pub fn schedule_keyed_periodic_event<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<ActionKey, SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let event_key = ActionKey::new();
        let sender = address.into().0;
        let channel_id = sender.channel_id();
        let action = Action::new(KeyedPeriodicAction::new(
            |ek| send_keyed_event(ek, func, arg, sender),
            period,
            event_key.clone(),
        ));
        scheduler_queue.insert((time, channel_id), action);

        Ok(event_key)
    }
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("time", &self.time())
            .finish_non_exhaustive()
    }
}

/// Local scheduler.
pub struct LocalScheduler<M: Model> {
    pub(crate) scheduler: Scheduler,
    address: Address<M>,
}

impl<M: Model> LocalScheduler<M> {
    pub(crate) fn new(scheduler: Scheduler, address: Address<M>) -> Self {
        Self { scheduler, address }
    }

    /// Returns the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::Model;
    /// use asynchronix::simulation::LocalScheduler;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// fn is_third_millenium<M: Model>(scheduler: &LocalScheduler<M>) -> bool {
    ///     let time = scheduler.time();
    ///     time >= MonotonicTime::new(978307200, 0).unwrap()
    ///         && time < MonotonicTime::new(32535216000, 0).unwrap()
    /// }
    /// ```
    pub fn time(&self) -> MonotonicTime {
        self.scheduler.time()
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
    /// use asynchronix::model::{Context, Model};
    ///
    /// // A timer.
    /// pub struct Timer {}
    ///
    /// impl Timer {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: Duration, context: &Context<Self>) {
    ///         if context.scheduler.schedule_event(setting, Self::ring, ()).is_err() {
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
        self.scheduler
            .schedule_event(deadline, func, arg, &self.address)
    }

    /// Schedules a cancellable event at a future time and returns an action
    /// key.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::model::{Context, Model};
    /// use asynchronix::simulation::ActionKey;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // An alarm clock that can be cancelled.
    /// #[derive(Default)]
    /// pub struct CancellableAlarmClock {
    ///     event_key: Option<ActionKey>,
    /// }
    ///
    /// impl CancellableAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, context: &Context<Self>) {
    ///         self.cancel();
    ///         match context.scheduler.schedule_keyed_event(setting, Self::ring, ()) {
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
    ) -> Result<ActionKey, SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let event_key = self
            .scheduler
            .schedule_keyed_event(deadline, func, arg, &self.address)?;

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
    /// use asynchronix::model::{Context, Model};
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // An alarm clock beeping at 1Hz.
    /// pub struct BeepingAlarmClock {}
    ///
    /// impl BeepingAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, context: &Context<Self>) {
    ///         if context.scheduler.schedule_periodic_event(
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
        self.scheduler
            .schedule_periodic_event(deadline, period, func, arg, &self.address)
    }

    /// Schedules a cancellable, periodically recurring event at a future time
    /// and returns an action key.
    ///
    /// An error is returned if the specified deadline is not in the future of
    /// the current simulation time or if the specified period is null.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use asynchronix::model::{Context, Model};
    /// use asynchronix::simulation::ActionKey;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // An alarm clock beeping at 1Hz that can be cancelled before it sets off, or
    /// // stopped after it sets off.
    /// #[derive(Default)]
    /// pub struct CancellableBeepingAlarmClock {
    ///     event_key: Option<ActionKey>,
    /// }
    ///
    /// impl CancellableBeepingAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, context: &Context<Self>) {
    ///         self.cancel();
    ///         match context.scheduler.schedule_keyed_periodic_event(
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
    ) -> Result<ActionKey, SchedulingError>
    where
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let event_key = self.scheduler.schedule_keyed_periodic_event(
            deadline,
            period,
            func,
            arg,
            &self.address,
        )?;

        Ok(event_key)
    }
}

impl<M: Model> Clone for LocalScheduler<M> {
    fn clone(&self) -> Self {
        Self {
            scheduler: self.scheduler.clone(),
            address: self.address.clone(),
        }
    }
}

impl<M: Model> fmt::Debug for LocalScheduler<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalScheduler")
            .field("time", &self.time())
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// Shorthand for the scheduler queue type.

// Why use both time and channel ID as the key? The short answer is that this
// ensures that events targeting the same model are sent in the order they were
// scheduled. More precisely, this ensures that events targeting the same model
// are ordered contiguously in the priority queue, which in turns allows the
// event loop to easily aggregate such events into single futures and thus
// control their relative order of execution.
pub(crate) type SchedulerQueue = PriorityQueue<(MonotonicTime, usize), Action>;

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

/// Managed handle to a scheduled action.
///
/// An `AutoActionKey` is a managed handle to a scheduled action that cancels
/// action on drop.
#[derive(Debug)]
#[must_use = "managed action key shall be used"]
pub struct AutoActionKey {
    is_cancelled: Arc<AtomicBool>,
}

impl Drop for AutoActionKey {
    fn drop(&mut self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
    }
}

/// Handle to a scheduled action.
///
/// An `ActionKey` can be used to cancel a scheduled action.
#[derive(Clone, Debug)]
#[must_use = "prefer unkeyed scheduling methods if the action is never cancelled"]
pub struct ActionKey {
    is_cancelled: Arc<AtomicBool>,
}

impl ActionKey {
    /// Creates a key for a pending action.
    pub(crate) fn new() -> Self {
        Self {
            is_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Checks whether the action was cancelled.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Relaxed)
    }

    /// Cancels the associated action.
    pub fn cancel(self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
    }

    /// Converts action key to a managed key.
    pub fn into_auto(self) -> AutoActionKey {
        AutoActionKey {
            is_cancelled: self.is_cancelled,
        }
    }
}

impl PartialEq for ActionKey {
    /// Implements equality by considering clones to be equivalent, rather than
    /// keys with the same `is_cancelled` value.
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(&*self.is_cancelled, &*other.is_cancelled)
    }
}

impl Eq for ActionKey {}

impl Hash for ActionKey {
    /// Implements `Hash`` by considering clones to be equivalent, rather than
    /// keys with the same `is_cancelled` value.
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        ptr::hash(&*self.is_cancelled, state)
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

/// A possibly periodic, possibly cancellable action that can be scheduled or
/// processed immediately.
pub struct Action {
    inner: Box<dyn ActionInner>,
}

impl Action {
    /// Creates a new `Action` from an `ActionInner`.
    pub(crate) fn new<S: ActionInner>(s: S) -> Self {
        Self { inner: Box::new(s) }
    }

    /// Reports whether the action was cancelled.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// If this is a periodic action, returns a boxed clone of this action and
    /// its repetition period; otherwise returns `None`.
    pub(crate) fn next(&self) -> Option<(Action, Duration)> {
        self.inner
            .next()
            .map(|(inner, period)| (Self { inner }, period))
    }

    /// Returns a boxed future that performs the action.
    pub(crate) fn into_future(self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.inner.into_future()
    }

    /// Spawns the future that performs the action onto the provided executor.
    ///
    /// This method is typically more efficient that spawning the boxed future
    /// from `into_future` since it can directly spawn the unboxed future.
    pub(crate) fn spawn_and_forget(self, executor: &Executor) {
        self.inner.spawn_and_forget(executor)
    }
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SchedulableEvent").finish_non_exhaustive()
    }
}

/// Trait abstracting over the inner type of an action.
pub(crate) trait ActionInner: Send + 'static {
    /// Reports whether the action was cancelled.
    fn is_cancelled(&self) -> bool;

    /// If this is a periodic action, returns a boxed clone of this action and
    /// its repetition period; otherwise returns `None`.
    fn next(&self) -> Option<(Box<dyn ActionInner>, Duration)>;

    /// Returns a boxed future that performs the action.
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Spawns the future that performs the action onto the provided executor.
    ///
    /// This method is typically more efficient that spawning the boxed future
    /// from `into_future` since it can directly spawn the unboxed future.
    fn spawn_and_forget(self: Box<Self>, executor: &Executor);
}

pin_project! {
    /// An object that can be converted to a future performing a single
    /// non-cancellable action.
    ///
    /// Note that this particular action is in fact already a future: since the
    /// future cannot be cancelled and the action does not need to be cloned,
    /// there is no need to defer the construction of the future. This makes
    /// `into_future` a trivial cast, which saves a boxing operation.
    pub(crate) struct OnceAction<F> {
        #[pin]
        fut: F,
    }
}

impl<F> OnceAction<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `OnceAction`.
    pub(crate) fn new(fut: F) -> Self {
        OnceAction { fut }
    }
}

impl<F> Future for OnceAction<F>
where
    F: Future,
{
    type Output = F::Output;

    #[inline(always)]
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

impl<F> ActionInner for OnceAction<F>
where
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        false
    }
    fn next(&self) -> Option<(Box<dyn ActionInner>, Duration)> {
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

/// An object that can be converted to a future performing a non-cancellable,
/// periodic action.
pub(crate) struct PeriodicAction<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A clonable generator for the associated future.
    gen: G,
    /// The action repetition period.
    period: Duration,
}

impl<G, F> PeriodicAction<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `PeriodicAction`.
    pub(crate) fn new(gen: G, period: Duration) -> Self {
        Self { gen, period }
    }
}

impl<G, F> ActionInner for PeriodicAction<G, F>
where
    G: (FnOnce() -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        false
    }
    fn next(&self) -> Option<(Box<dyn ActionInner>, Duration)> {
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

/// An object that can be converted to a future performing a single, cancellable
/// action.
pub(crate) struct KeyedOnceAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A generator for the associated future.
    gen: G,
    /// The event cancellation key.
    event_key: ActionKey,
}

impl<G, F> KeyedOnceAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `KeyedOnceAction`.
    pub(crate) fn new(gen: G, event_key: ActionKey) -> Self {
        Self { gen, event_key }
    }
}

impl<G, F> ActionInner for KeyedOnceAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        self.event_key.is_cancelled()
    }
    fn next(&self) -> Option<(Box<dyn ActionInner>, Duration)> {
        None
    }
    fn into_future(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin((self.gen)(self.event_key))
    }
    fn spawn_and_forget(self: Box<Self>, executor: &Executor) {
        executor.spawn_and_forget((self.gen)(self.event_key));
    }
}

/// An object that can be converted to a future performing a periodic,
/// cancellable action.
pub(crate) struct KeyedPeriodicAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// A clonable generator for associated future.
    gen: G,
    /// The repetition period.
    period: Duration,
    /// The event cancellation key.
    event_key: ActionKey,
}

impl<G, F> KeyedPeriodicAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    /// Constructs a new `KeyedPeriodicAction`.
    pub(crate) fn new(gen: G, period: Duration, event_key: ActionKey) -> Self {
        Self {
            gen,
            period,
            event_key,
        }
    }
}

impl<G, F> ActionInner for KeyedPeriodicAction<G, F>
where
    G: (FnOnce(ActionKey) -> F) + Clone + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    fn is_cancelled(&self) -> bool {
        self.event_key.is_cancelled()
    }
    fn next(&self) -> Option<(Box<dyn ActionInner>, Duration)> {
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

/// Asynchronously sends a non-cancellable event to a model input.
pub(crate) async fn process_event<M, F, T, S>(func: F, arg: T, sender: Sender<M>)
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

/// Asynchronously sends a cancellable event to a model input.
pub(crate) async fn send_keyed_event<M, F, T, S>(
    event_key: ActionKey,
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
