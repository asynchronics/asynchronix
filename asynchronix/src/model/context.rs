use std::fmt;
use std::time::Duration;

use crate::executor::{Executor, Signal};
use crate::ports::InputFn;
use crate::simulation::{self, ActionKey, Address, GlobalScheduler, Mailbox, SchedulingError};
use crate::time::{Deadline, MonotonicTime};

use super::{Model, ProtoModel};

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
///     cx: &'a mut Context<Self>
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
///     pub async fn greet_with_delay(&mut self, delay: Duration, cx: &mut Context<Self>) {
///         let time = cx.time();
///         let greeting = format!("Hello, this message was scheduled at: {:?}.", time);
///
///         if delay.is_zero() {
///             self.msg_out.send(greeting).await;
///         } else {
///             cx.schedule_event(delay, Self::send_msg, greeting).unwrap();
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
    name: String,
    scheduler: GlobalScheduler,
    address: Address<M>,
    origin_id: usize,
}

impl<M: Model> Context<M> {
    /// Creates a new local context.
    pub(crate) fn new(name: String, scheduler: GlobalScheduler, address: Address<M>) -> Self {
        // The only requirement for the origin ID is that it must be (i)
        // specific to each model and (ii) different from 0 (which is reserved
        // for the global scheduler). The channel ID of the model mailbox
        // fulfills this requirement.
        let origin_id = address.0.channel_id();

        Self {
            name,
            scheduler,
            address,
            origin_id,
        }
    }

    /// Returns the fully qualified model instance name.
    ///
    /// The fully qualified name is made of the unqualified model name, if
    /// relevant prepended by the dot-separated names of all parent models.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the current simulation time.
    pub fn time(&self) -> MonotonicTime {
        self.scheduler.time()
    }

    /// Schedules an event at a future time on this model.
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
    ///     pub fn set(&mut self, setting: Duration, cx: &mut Context<Self>) {
    ///         if cx.schedule_event(setting, Self::ring, ()).is_err() {
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

    /// Schedules a cancellable event at a future time on this model and returns
    /// an action key.
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
    ///     pub fn set(&mut self, setting: MonotonicTime, cx: &mut Context<Self>) {
    ///         self.cancel();
    ///         match cx.schedule_keyed_event(setting, Self::ring, ()) {
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

    /// Schedules a periodically recurring event on this model at a future time.
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
    ///     pub fn set(&mut self, setting: MonotonicTime, cx: &mut Context<Self>) {
    ///         if cx.schedule_periodic_event(
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

    /// Schedules a cancellable, periodically recurring event on this model at a
    /// future time and returns an action key.
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
    ///     pub fn set(&mut self, setting: MonotonicTime, cx: &mut Context<Self>) {
    ///         self.cancel();
    ///         match cx.schedule_keyed_periodic_event(
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

impl<M: Model> fmt::Debug for Context<M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Context")
            .field("name", &self.name())
            .field("time", &self.time())
            .field("address", &self.address)
            .field("origin_id", &self.origin_id)
            .finish_non_exhaustive()
    }
}

/// Context available when building a model from a model prototype.
///
/// A `BuildContext` can be used to add the sub-models of a hierarchical model
/// to the simulation bench.
///
/// # Examples
///
/// A model that multiplies its input by four using two sub-models that each
/// multiply their input by two.
///
/// ```text
///             ┌───────────────────────────────────────┐
///             │ MyltiplyBy4                           │
///             │   ┌─────────────┐   ┌─────────────┐   │
///             │   │             │   │             │   │
/// Input ●─────┼──►│ MultiplyBy2 ├──►│ MultiplyBy2 ├───┼─────► Output
///         f64 │   │             │   │             │   │ f64
///             │   └─────────────┘   └─────────────┘   │
///             │                                       │
///             └───────────────────────────────────────┘
/// ```
///
/// ```
/// use std::time::Duration;
/// use asynchronix::model::{BuildContext, Model, ProtoModel};
/// use asynchronix::ports::Output;
/// use asynchronix::simulation::Mailbox;
///
/// #[derive(Default)]
/// struct MultiplyBy2 {
///     pub output: Output<i32>,
/// }
/// impl MultiplyBy2 {
///     pub async fn input(&mut self, value: i32) {
///         self.output.send(value * 2).await;
///     }
/// }
/// impl Model for MultiplyBy2 {}
///
/// pub struct MultiplyBy4 {
///     // Private forwarding output.
///     forward: Output<i32>,
/// }
/// impl MultiplyBy4 {
///     pub async fn input(&mut self, value: i32) {
///         self.forward.send(value).await;
///     }
/// }
/// impl Model for MultiplyBy4 {}
///
/// pub struct ProtoMultiplyBy4 {
///     pub output: Output<i32>,
/// }
/// impl ProtoModel for ProtoMultiplyBy4 {
///     type Model = MultiplyBy4;
///
///     fn build(
///         self,
///         cx: &mut BuildContext<Self>)
///     -> MultiplyBy4 {
///         let mut mult = MultiplyBy4 { forward: Output::default() };
///         let mut submult1 = MultiplyBy2::default();
///
///         // Move the prototype's output to the second multiplier.
///         let mut submult2 = MultiplyBy2 { output: self.output };
///
///         // Forward the parent's model input to the first multiplier.
///         let submult1_mbox = Mailbox::new();
///         mult.forward.connect(MultiplyBy2::input, &submult1_mbox);
///         
///         // Connect the two multiplier submodels.
///         let submult2_mbox = Mailbox::new();
///         submult1.output.connect(MultiplyBy2::input, &submult2_mbox);
///         
///         // Add the submodels to the simulation.
///         cx.add_submodel(submult1, submult1_mbox, "submultiplier 1");
///         cx.add_submodel(submult2, submult2_mbox, "submultiplier 2");
///
///         mult
///     }
/// }
///
/// ```
#[derive(Debug)]
pub struct BuildContext<'a, P: ProtoModel> {
    mailbox: &'a Mailbox<P::Model>,
    name: &'a String,
    scheduler: &'a GlobalScheduler,
    executor: &'a Executor,
    abort_signal: &'a Signal,
    model_names: &'a mut Vec<String>,
}

impl<'a, P: ProtoModel> BuildContext<'a, P> {
    /// Creates a new local context.
    pub(crate) fn new(
        mailbox: &'a Mailbox<P::Model>,
        name: &'a String,
        scheduler: &'a GlobalScheduler,
        executor: &'a Executor,
        abort_signal: &'a Signal,
        model_names: &'a mut Vec<String>,
    ) -> Self {
        Self {
            mailbox,
            name,
            scheduler,
            executor,
            abort_signal,
            model_names,
        }
    }

    /// Returns the fully qualified model instance name.
    ///
    /// The fully qualified name is made of the unqualified model name, if
    /// relevant prepended by the dot-separated names of all parent models.
    pub fn name(&self) -> &str {
        self.name
    }

    /// Returns a handle to the model's mailbox.
    pub fn address(&self) -> Address<P::Model> {
        self.mailbox.address()
    }

    /// Adds a sub-model to the simulation bench.
    ///
    /// The `name` argument needs not be unique. It is appended to that of the
    /// parent models' names using a dot separator (e.g.
    /// `parent_name.child_name`) to build the fully qualified name. The use of
    /// the dot character in the unqualified name is possible but discouraged.
    /// If an empty string is provided, it is replaced by the string
    /// `<unknown>`.
    pub fn add_submodel<S: ProtoModel>(
        &mut self,
        model: S,
        mailbox: Mailbox<S::Model>,
        name: impl Into<String>,
    ) {
        let mut submodel_name = name.into();
        if submodel_name.is_empty() {
            submodel_name = String::from("<unknown>");
        };
        submodel_name = self.name.to_string() + "." + &submodel_name;

        simulation::add_model(
            model,
            mailbox,
            submodel_name,
            self.scheduler.clone(),
            self.executor,
            self.abort_signal,
            self.model_names,
        );
    }
}
