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

use pin_project::pin_project;
use recycle_box::{coerce_box, RecycleBox};

use crate::channel::Sender;
use crate::executor::Executor;
use crate::model::Model;
use crate::ports::InputFn;
use crate::simulation::Address;
use crate::time::{AtomicTimeReader, Deadline, MonotonicTime};
use crate::util::priority_queue::PriorityQueue;

const GLOBAL_SCHEDULER_ORIGIN_ID: usize = 0;

/// A global scheduler.
#[derive(Clone)]
pub struct Scheduler(GlobalScheduler);

impl Scheduler {
    pub(crate) fn new(scheduler_queue: Arc<Mutex<SchedulerQueue>>, time: AtomicTimeReader) -> Self {
        Self(GlobalScheduler::new(scheduler_queue, time))
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
        self.0.time()
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
        self.0
            .schedule_from(deadline, action, GLOBAL_SCHEDULER_ORIGIN_ID)
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
        self.0
            .schedule_event_from(deadline, func, arg, address, GLOBAL_SCHEDULER_ORIGIN_ID)
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
        self.0
            .schedule_keyed_event_from(deadline, func, arg, address, GLOBAL_SCHEDULER_ORIGIN_ID)
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
        self.0.schedule_periodic_event_from(
            deadline,
            period,
            func,
            arg,
            address,
            GLOBAL_SCHEDULER_ORIGIN_ID,
        )
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
        self.0.schedule_keyed_periodic_event_from(
            deadline,
            period,
            func,
            arg,
            address,
            GLOBAL_SCHEDULER_ORIGIN_ID,
        )
    }
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("time", &self.time())
            .finish_non_exhaustive()
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

/// Alias for the scheduler queue type.
///
/// Why use both time and origin ID as the key? The short answer is that this
/// allows to preserve the relative ordering of events which have the same
/// origin (where the origin is either a model instance or the global
/// scheduler). The preservation of this ordering is implemented by the event
/// loop, which aggregate events with the same origin into single sequential
/// futures, thus ensuring that they are not executed concurrently.
pub(crate) type SchedulerQueue = PriorityQueue<(MonotonicTime, usize), Action>;

/// Internal implementation of the global scheduler.
#[derive(Clone)]
pub(crate) struct GlobalScheduler {
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: AtomicTimeReader,
}

impl GlobalScheduler {
    pub(crate) fn new(scheduler_queue: Arc<Mutex<SchedulerQueue>>, time: AtomicTimeReader) -> Self {
        Self {
            scheduler_queue,
            time,
        }
    }

    /// Returns the current simulation time.
    pub(crate) fn time(&self) -> MonotonicTime {
        // We use `read` rather than `try_read` because the scheduler can be
        // sent to another thread than the simulator's and could thus
        // potentially see a torn read if the simulator increments time
        // concurrently. The chances of this happening are very small since
        // simulation time is not changed frequently.
        self.time.read()
    }

    /// Schedules an action identified by its origin at a future time.
    pub(crate) fn schedule_from(
        &self,
        deadline: impl Deadline,
        action: Action,
        origin_id: usize,
    ) -> Result<(), SchedulingError> {
        // The scheduler queue must always be locked when reading the time,
        // otherwise the following race could occur:
        // 1) this method reads the time and concludes that it is not too late
        //    to schedule the action,
        // 2) the `Simulation` object takes the lock, increments simulation time
        //    and runs the simulation step,
        // 3) this method takes the lock and schedules the now-outdated action.
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();

        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        scheduler_queue.insert((time, origin_id), action);

        Ok(())
    }

    /// Schedules an event identified by its origin at a future time.
    pub(crate) fn schedule_event_from<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
        origin_id: usize,
    ) -> Result<(), SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let sender = address.into().0;
        let action = Action::new(OnceAction::new(process_event(func, arg, sender)));

        // The scheduler queue must always be locked when reading the time (see
        // `schedule_from`).
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        scheduler_queue.insert((time, origin_id), action);

        Ok(())
    }

    /// Schedules a cancellable event identified by its origin at a future time
    /// and returns an event key.
    pub(crate) fn schedule_keyed_event_from<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
        origin_id: usize,
    ) -> Result<ActionKey, SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        let event_key = ActionKey::new();
        let sender = address.into().0;
        let action = Action::new(KeyedOnceAction::new(
            |ek| send_keyed_event(ek, func, arg, sender),
            event_key.clone(),
        ));

        // The scheduler queue must always be locked when reading the time (see
        // `schedule_from`).
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        scheduler_queue.insert((time, origin_id), action);

        Ok(event_key)
    }

    /// Schedules a periodically recurring event identified by its origin at a
    /// future time.
    pub(crate) fn schedule_periodic_event_from<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
        origin_id: usize,
    ) -> Result<(), SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let sender = address.into().0;
        let action = Action::new(PeriodicAction::new(
            || process_event(func, arg, sender),
            period,
        ));

        // The scheduler queue must always be locked when reading the time (see
        // `schedule_from`).
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        scheduler_queue.insert((time, origin_id), action);

        Ok(())
    }

    /// Schedules a cancellable, periodically recurring event identified by its
    /// origin at a future time and returns an event key.
    pub(crate) fn schedule_keyed_periodic_event_from<M, F, T, S>(
        &self,
        deadline: impl Deadline,
        period: Duration,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
        origin_id: usize,
    ) -> Result<ActionKey, SchedulingError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S> + Clone,
        T: Send + Clone + 'static,
        S: Send + 'static,
    {
        if period.is_zero() {
            return Err(SchedulingError::NullRepetitionPeriod);
        }
        let event_key = ActionKey::new();
        let sender = address.into().0;
        let action = Action::new(KeyedPeriodicAction::new(
            |ek| send_keyed_event(ek, func, arg, sender),
            period,
            event_key.clone(),
        ));

        // The scheduler queue must always be locked when reading the time (see
        // `schedule_from`).
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let now = self.time();
        let time = deadline.into_time(now);
        if now >= time {
            return Err(SchedulingError::InvalidScheduledTime);
        }

        scheduler_queue.insert((time, origin_id), action);

        Ok(event_key)
    }
}

impl fmt::Debug for GlobalScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerInner")
            .field("time", &self.time())
            .finish_non_exhaustive()
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

/// An object that can be converted to a future performing a single
/// non-cancellable action.
///
/// Note that this particular action is in fact already a future: since the
/// future cannot be cancelled and the action does not need to be cloned,
/// there is no need to defer the construction of the future. This makes
/// `into_future` a trivial cast, which saves a boxing operation.
#[pin_project]
pub(crate) struct OnceAction<F> {
    #[pin]
    fut: F,
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
