//! Scheduling functions and types.
mod inner;

use std::error::Error;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fmt, ptr};

use crate::executor::Executor;
use crate::model::Model;
use crate::ports::InputFn;
use crate::simulation::Address;
use crate::time::{AtomicTimeReader, Deadline, MonotonicTime};

use inner::ActionInner;

pub(crate) use inner::{
    KeyedOnceAction, KeyedPeriodicAction, OnceAction, PeriodicAction, SchedulerInner,
    SchedulerQueue,
};

const GLOBAL_SCHEDULER_ORIGIN_ID: usize = 0;

/// A global scheduler.
#[derive(Clone)]
pub struct Scheduler(SchedulerInner);

impl Scheduler {
    pub(crate) fn new(scheduler_queue: Arc<Mutex<SchedulerQueue>>, time: AtomicTimeReader) -> Self {
        Self(SchedulerInner::new(scheduler_queue, time))
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

/// Local scheduler.
pub struct LocalScheduler<M: Model> {
    scheduler: SchedulerInner,
    address: Address<M>,
    origin_id: usize,
}

impl<M: Model> LocalScheduler<M> {
    pub(crate) fn new(scheduler: SchedulerInner, address: Address<M>) -> Self {
        // The only requirement for the origin ID is that it must be (i)
        // specific to each model and (ii) different from 0 (which is reserved
        // for the global scheduler). The channel ID of the model mailbox
        // fulfills this requirement.
        let origin_id = address.0.channel_id();

        Self {
            scheduler,
            address,
            origin_id,
        }
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
            .schedule_event_from(deadline, func, arg, &self.address, self.origin_id)
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
        let event_key = self.scheduler.schedule_keyed_event_from(
            deadline,
            func,
            arg,
            &self.address,
            self.origin_id,
        )?;

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
        self.scheduler.schedule_periodic_event_from(
            deadline,
            period,
            func,
            arg,
            &self.address,
            self.origin_id,
        )
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
        let event_key = self.scheduler.schedule_keyed_periodic_event_from(
            deadline,
            period,
            func,
            arg,
            &self.address,
            self.origin_id,
        )?;

        Ok(event_key)
    }
}

impl<M: Model> Clone for LocalScheduler<M> {
    fn clone(&self) -> Self {
        Self {
            scheduler: self.scheduler.clone(),
            address: self.address.clone(),
            origin_id: self.origin_id,
        }
    }
}

impl<M: Model> fmt::Debug for LocalScheduler<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalScheduler")
            .field("time", &self.time())
            .field("address", &self.address)
            .field("origin_id", &self.origin_id)
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
