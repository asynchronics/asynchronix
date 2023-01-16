//! Scheduling functions and types.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use recycle_box::{coerce_box, RecycleBox};

use crate::channel::{ChannelId, Sender};
use crate::model::{InputFn, Model};
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::priority_queue::{self, PriorityQueue};
use crate::util::sync_cell::SyncCellReader;

/// Shorthand for the scheduler queue type.
pub(crate) type SchedulerQueue =
    PriorityQueue<(MonotonicTime, ChannelId), Box<dyn Future<Output = ()> + Send>>;

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
///         if let Err(err) = scheduler.schedule_in(delay, Self::send_msg, greeting) {
///             //                                               ^^^^^^^^ scheduled method
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
    ///             .schedule_in(Duration::from_secs(1), Self::trigger, counter + 1)
    ///             .unwrap();
    ///     }
    /// }
    ///
    /// impl Model for PeriodicLogger {}
    /// ```
    pub fn schedule_in<F, T, S>(
        &self,
        duration: Duration,
        func: F,
        arg: T,
    ) -> Result<SchedulerKey, ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if duration.is_zero() {
            return Err(ScheduledTimeError(arg));
        }
        let time = self.time() + duration;
        let sender = self.sender.clone();
        let schedule_key =
            schedule_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(schedule_key)
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
    /// // An alarm clock model.
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
    ///         if scheduler.schedule_at(setting, Self::ring, ()).is_err() {
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
    pub fn schedule_at<F, T, S>(
        &self,
        time: MonotonicTime,
        func: F,
        arg: T,
    ) -> Result<SchedulerKey, ScheduledTimeError<T>>
    where
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        if self.time() >= time {
            return Err(ScheduledTimeError(arg));
        }
        let sender = self.sender.clone();
        let schedule_key =
            schedule_event_at_unchecked(time, func, arg, sender, &self.scheduler_queue);

        Ok(schedule_key)
    }

    /// Cancels an event with a scheduled time in the future of the current
    /// simulation time.
    ///
    /// If the corresponding event was already executed, or if it is scheduled
    /// for the current simulation time but was not yet executed, an error is
    /// returned.
    pub fn cancel(&self, scheduler_key: SchedulerKey) -> Result<(), CancellationError> {
        cancel_scheduled(scheduler_key, &self.scheduler_queue)
    }
}

impl<M: Model> fmt::Debug for Scheduler<M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

/// Unique identifier for a scheduled event.
///
/// A `SchedulerKey` can be used to cancel a future event.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct SchedulerKey(priority_queue::InsertKey);

impl SchedulerKey {
    pub(crate) fn new(key: priority_queue::InsertKey) -> Self {
        Self(key)
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

/// Error returned when the cancellation of a scheduler event is unsuccessful.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct CancellationError {}

impl fmt::Display for CancellationError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "the scheduler key should belong to an event or command scheduled in the future of the current simulation time"
        )
    }
}

impl Error for CancellationError {}

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
) -> SchedulerKey
where
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

    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    let insert_key = scheduler_queue.insert((time, channel_id), Box::new(fut));

    SchedulerKey::new(insert_key)
}

/// Cancels an event or command with a scheduled time in the future of the
/// current simulation time.
///
/// If the corresponding event or command was already executed, or if it is
/// scheduled for the current simulation time, an error is returned.
pub(crate) fn cancel_scheduled(
    scheduler_key: SchedulerKey,
    scheduler_queue: &Mutex<SchedulerQueue>,
) -> Result<(), CancellationError> {
    let mut scheduler_queue = scheduler_queue.lock().unwrap();
    if scheduler_queue.delete(scheduler_key.0) {
        return Ok(());
    }

    Err(CancellationError {})
}
