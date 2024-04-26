use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::channel::Sender;
use crate::executor::Executor;
use crate::ports::InputFn;
use crate::simulation::{
    self, schedule_event_at_unchecked, schedule_keyed_event_at_unchecked,
    schedule_periodic_event_at_unchecked, schedule_periodic_keyed_event_at_unchecked, ActionKey,
    Deadline, Mailbox, SchedulerQueue, SchedulingError,
};
use crate::time::{MonotonicTime, TearableAtomicTime};
use crate::util::sync_cell::SyncCellReader;

use super::Model;

/// A local context for models.
///
/// A `Context` is a handle to the global context associated to a model
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
///     context: &'a Context<Self>
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
/// use asynchronix::model::{Context, Model};
/// use asynchronix::ports::Output;
///
/// #[derive(Default)]
/// pub struct DelayedGreeter {
///     msg_out: Output<String>,
/// }
///
/// impl DelayedGreeter {
///     // Triggers a greeting on the output port after some delay [input port].
///     pub async fn greet_with_delay(&mut self, delay: Duration, context: &Context<Self>) {
///         let time = context.time();
///         let greeting = format!("Hello, this message was scheduled at: {:?}.", time);
///
///         if delay.is_zero() {
///             self.msg_out.send(greeting).await;
///         } else {
///             context.schedule_event(delay, Self::send_msg, greeting).unwrap();
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
pub struct Context<M: Model> {
    sender: Sender<M>,
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: SyncCellReader<TearableAtomicTime>,
}

impl<M: Model> Context<M> {
    /// Creates a new local context.
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
    /// use asynchronix::model::{Context, Model};
    /// use asynchronix::time::MonotonicTime;
    ///
    /// fn is_third_millenium<M: Model>(context: &Context<M>) -> bool {
    ///     let time = context.time();
    ///     time >= MonotonicTime::new(978307200, 0).unwrap()
    ///         && time < MonotonicTime::new(32535216000, 0).unwrap()
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
    /// use asynchronix::model::{Context, Model};
    ///
    /// // A timer.
    /// pub struct Timer {}
    ///
    /// impl Timer {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: Duration, context: &Context<Self>) {
    ///         if context.schedule_event(setting, Self::ring, ()).is_err() {
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
    ///         match context.schedule_keyed_event(setting, Self::ring, ()) {
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
    /// use asynchronix::model::{Context, Model};
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // An alarm clock beeping at 1Hz.
    /// pub struct BeepingAlarmClock {}
    ///
    /// impl BeepingAlarmClock {
    ///     // Sets an alarm [input port].
    ///     pub fn set(&mut self, setting: MonotonicTime, context: &Context<Self>) {
    ///         if context.schedule_periodic_event(
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
    ///         match context.schedule_keyed_periodic_event(
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

impl<M: Model> fmt::Debug for Context<M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// A setup context for models.
///
/// A `SetupContext` can be used by models during the setup stage to
/// create submodels and add them to the simulation bench.
///
/// # Examples
///
/// A model that contains two connected submodels.
///
/// ```
/// use std::time::Duration;
/// use asynchronix::model::{Model, SetupContext};
/// use asynchronix::ports::Output;
/// use asynchronix::simulation::Mailbox;
///
/// #[derive(Default)]
/// pub struct SubmodelA {
///     out: Output<u32>,
/// }
///
/// impl Model for SubmodelA {}
///
/// #[derive(Default)]
/// pub struct SubmodelB {}
///
/// impl SubmodelB {
///     pub async fn input(&mut self, value: u32) {
///         println!("Received {}", value);
///     }
/// }
///
/// impl Model for SubmodelB {}
///
/// #[derive(Default)]
/// pub struct Parent {}
///
/// impl Model for Parent {
///     fn setup(
///        &mut self,
///        setup_context: &SetupContext<Self>) {
///            let mut a = SubmodelA::default();
///            let b = SubmodelB::default();
///            let a_mbox = Mailbox::new();
///            let b_mbox = Mailbox::new();
///
///            a.out.connect(SubmodelB::input, &b_mbox);
///
///            setup_context.add_model(a, a_mbox);
///            setup_context.add_model(b, b_mbox);
///    }
/// }
///
/// ```

#[derive(Debug)]
pub struct SetupContext<'a, M: Model> {
    /// Mailbox of the model.
    pub mailbox: &'a Mailbox<M>,
    context: &'a Context<M>,
    executor: &'a Executor,
}

impl<'a, M: Model> SetupContext<'a, M> {
    /// Creates a new local context.
    pub(crate) fn new(
        mailbox: &'a Mailbox<M>,
        context: &'a Context<M>,
        executor: &'a Executor,
    ) -> Self {
        Self {
            mailbox,
            context,
            executor,
        }
    }

    /// Adds a new model and its mailbox to the simulation bench.
    pub fn add_model<N: Model>(&self, model: N, mailbox: Mailbox<N>) {
        simulation::add_model(
            model,
            mailbox,
            self.context.scheduler_queue.clone(),
            self.context.time.clone(),
            self.executor,
        );
    }
}
