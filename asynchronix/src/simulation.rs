//! Discrete-event simulation management.
//!
//! This module contains most notably the [`Simulation`] environment, the
//! [`SimInit`] simulation builder, the [`Mailbox`] and [`Address`] types as
//! well as miscellaneous other types related to simulation management.
//!
//! # Simulation lifecycle
//!
//! The lifecycle of a simulation bench typically comprises the following
//! stages:
//!
//! 1. instantiation of models and their [`Mailbox`]es,
//! 2. connection of the models' output/requestor ports to input/replier ports
//!    using the [`Address`]es of the target models,
//! 3. instantiation of a [`SimInit`] simulation builder and migration of all
//!    models and mailboxes to the builder with [`SimInit::add_model()`],
//! 4. initialization of a [`Simulation`] instance with [`SimInit::init()`],
//!    possibly preceded by the setup of a custom clock with
//!    [`SimInit::set_clock()`],
//! 5. discrete-time simulation, which typically involves scheduling events and
//!    incrementing simulation time while observing the models outputs.
//!
//! Most information necessary to run a simulation is available in the root
//! crate [documentation](crate) and in the [`SimInit`] and [`Simulation`]
//! documentation. The next section complement this information with a set of
//! practical recommendations that can help run and troubleshoot simulations.
//!
//! # Practical considerations
//!
//! ## Mailbox capacity
//!
//! A [`Mailbox`] is a buffer that store incoming events and queries for a
//! single model instance. Mailboxes have a bounded capacity, which defaults to
//! [`Mailbox::DEFAULT_CAPACITY`].
//!
//! The capacity is a trade-off: too large a capacity may lead to excessive
//! memory usage, whereas too small a capacity can hamper performance and
//! increase the likelihood of deadlocks (see next section). Note that, because
//! a mailbox may receive events or queries of various sizes, it is actually the
//! largest message sent that ultimately determines the amount of allocated
//! memory.
//!
//! The default capacity should prove a reasonable trade-off in most cases, but
//! for situations where it is not appropriate, it is possible to instantiate
//! mailboxes with a custom capacity by using [`Mailbox::with_capacity()`]
//! instead of [`Mailbox::new()`].
//!
//! ## Avoiding deadlocks
//!
//! While the underlying architecture of Asynchronix—the actor model—should
//! prevent most race conditions (including obviously data races which are not
//! possible in safe Rust) it is still possible in theory to generate deadlocks.
//! Though rare in practice, these may occur due to one of the below:
//!
//! 1. *query loopback*: if a model sends a query which is further forwarded by
//!    other models until it loops back to the initial model, that model would
//!    in effect wait for its own response and block,
//! 2. *mailbox saturation*: if several models concurrently send to one another
//!    a very large number of messages in succession, these models may end up
//!    saturating all mailboxes, at which point they will wait for the other's
//!    mailboxes to free space so they can send the next message, eventually
//!    preventing all of them to make further progress.
//!
//! The first scenario is usually very easy to avoid and is typically the result
//! of an improper assembly of models. Because requestor ports are only used
//! sparingly in idiomatic simulations, this situation should be relatively
//! exceptional.
//!
//! The second scenario is rare in well-behaving models and if it occurs, it is
//! most typically at the very beginning of a simulation when all models
//! simultaneously send events during the call to
//! [`Model::init()`](crate::model::Model::init). If such a large amount of
//! concurrent messages is deemed normal behavior, the issue can be readily
//! remedied by increasing the capacity of the saturated mailboxes.
//!
//! At the moment, Asynchronix is unfortunately not able to discriminate between
//! such pathological deadlocks and the "expected" deadlock that occurs when all
//! events in a given time slice have completed and all models are starved on an
//! empty mailbox. Consequently, blocking method such as [`SimInit::init()`],
//! [`Simulation::step()`], [`Simulation::process_event()`], etc., will return
//! without error after a pathological deadlock, leaving the user responsible
//! for inferring the deadlock from the behavior of the simulation in the next
//! steps. This is obviously not ideal, but is hopefully only a temporary state
//! of things until a more precise deadlock detection algorithm is implemented.
//!
//! ## Modifying connections during simulation
//!
//! Although uncommon, there is sometimes a need for connecting and/or
//! disconnecting models after they have been migrated to the simulation.
//! Likewise, one may want to connect or disconnect an
//! [`EventSlot`](crate::ports::EventSlot) or
//! [`EventBuffer`](crate::ports::EventBuffer) after the simulation has been
//! instantiated.
//!
//! There is actually a very simple solution to this problem: since the
//! [`InputFn`] trait also matches closures of type `FnOnce(&mut impl Model)`,
//! it is enough to invoke [`Simulation::process_event()`] with a closure that
//! connects or disconnects a port, such as:
//!
//! ```
//! # use asynchronix::model::{Context, Model};
//! # use asynchronix::ports::Output;
//! # use asynchronix::time::MonotonicTime;
//! # use asynchronix::simulation::{Mailbox, SimInit};
//! # pub struct ModelA {
//! #     pub output: Output<i32>,
//! # }
//! # impl Model for ModelA {};
//! # pub struct ModelB {}
//! # impl ModelB {
//! #     pub fn input(&mut self, value: i32) {}
//! # }
//! # impl Model for ModelB {};
//! # let modelA_addr = Mailbox::<ModelA>::new().address();
//! # let modelB_addr = Mailbox::<ModelB>::new().address();
//! # let mut simu = SimInit::new().init(MonotonicTime::EPOCH)?;
//! simu.process_event(
//!     |m: &mut ModelA| {
//!         m.output.connect(ModelB::input, modelB_addr);
//!     },
//!     (),
//!     &modelA_addr
//! )?;
//! # Ok::<(), asynchronix::simulation::SimulationError>(())
//! ```
mod mailbox;
mod scheduler;
mod sim_init;

pub use mailbox::{Address, Mailbox};
pub use scheduler::{
    Action, ActionKey, AutoActionKey, Deadline, LocalScheduler, Scheduler, SchedulingError,
};
pub(crate) use scheduler::{
    KeyedOnceAction, KeyedPeriodicAction, OnceAction, PeriodicAction, SchedulerQueue,
};
pub use sim_init::SimInit;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use recycle_box::{coerce_box, RecycleBox};

use crate::channel::ChannelObserver;
use crate::executor::{Executor, ExecutorError, Signal};
use crate::model::{BuildContext, Context, Model, ProtoModel};
use crate::ports::{InputFn, ReplierFn};
use crate::time::{AtomicTime, Clock, MonotonicTime, SyncStatus};
use crate::util::seq_futures::SeqFuture;
use crate::util::slot;

/// Simulation environment.
///
/// A `Simulation` is created by calling
/// [`SimInit::init()`](crate::simulation::SimInit::init) on a simulation
/// initializer. It contains an asynchronous executor that runs all simulation
/// models added beforehand to [`SimInit`].
///
/// A [`Simulation`] object also manages an event scheduling queue and
/// simulation time. The scheduling queue can be accessed from the simulation
/// itself, but also from models via the optional
/// [`&Context`](crate::model::Context) argument of input and replier port
/// methods.  Likewise, simulation time can be accessed with the
/// [`Simulation::time()`] method, or from models with the
/// [`LocalScheduler::time()`](crate::simulation::LocalScheduler::time) method.
///
/// Events and queries can be scheduled immediately, *i.e.* for the current
/// simulation time, using [`process_event()`](Simulation::process_event) and
/// [`send_query()`](Simulation::process_query). Calling these methods will
/// block until all computations triggered by such event or query have
/// completed. In the case of queries, the response is returned.
///
/// Events can also be scheduled at a future simulation time using one of the
/// [`schedule_*()`](Scheduler::schedule_event) method. These methods queue an
/// event without blocking.
///
/// Finally, the [`Simulation`] instance manages simulation time. A call to
/// [`step()`](Simulation::step) will:
///
/// 1. increment simulation time until that of the next scheduled event in
///    chronological order, then
/// 2. call [`Clock::synchronize()`](crate::time::Clock::synchronize) which, unless the
///    simulation is configured to run as fast as possible, blocks until the
///    desired wall clock time, and finally
/// 3. run all computations scheduled for the new simulation time.
///
/// The [`step_by()`](Simulation::step_by) and
/// [`step_until()`](Simulation::step_until) methods operate similarly but
/// iterate until the target simulation time has been reached.
pub struct Simulation {
    executor: Executor,
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: AtomicTime,
    clock: Box<dyn Clock>,
    clock_tolerance: Option<Duration>,
    timeout: Duration,
    observers: Vec<(String, Box<dyn ChannelObserver>)>,
    is_terminated: bool,
}

impl Simulation {
    /// Creates a new `Simulation` with the specified clock.
    pub(crate) fn new(
        executor: Executor,
        scheduler_queue: Arc<Mutex<SchedulerQueue>>,
        time: AtomicTime,
        clock: Box<dyn Clock + 'static>,
        clock_tolerance: Option<Duration>,
        timeout: Duration,
        observers: Vec<(String, Box<dyn ChannelObserver>)>,
    ) -> Self {
        Self {
            executor,
            scheduler_queue,
            time,
            clock,
            clock_tolerance,
            timeout,
            observers,
            is_terminated: false,
        }
    }

    /// Sets a timeout for each simulation step.
    ///
    /// The timeout corresponds to the maximum wall clock time allocated for the
    /// completion of a single simulation step before an
    /// [`ExecutionError::Timeout`] error is raised.
    ///
    /// A null duration disables the timeout, which is the default behavior.
    ///
    /// See also [`SimInit::set_timeout`].
    #[cfg(not(target_family = "wasm"))]
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Returns the current simulation time.
    pub fn time(&self) -> MonotonicTime {
        self.time.read()
    }

    /// Advances simulation time to that of the next scheduled event, processing
    /// that event as well as all other events scheduled for the same time.
    ///
    /// Processing is gated by a (possibly blocking) call to
    /// [`Clock::synchronize()`](crate::time::Clock::synchronize) on the configured
    /// simulation clock. This method blocks until all newly processed events
    /// have completed.
    pub fn step(&mut self) -> Result<(), ExecutionError> {
        self.step_to_next_bounded(MonotonicTime::MAX).map(|_| ())
    }

    /// Iteratively advances the simulation time by the specified duration, as
    /// if by calling [`Simulation::step()`] repeatedly.
    ///
    /// This method blocks until all events scheduled up to the specified target
    /// time have completed. The simulation time upon completion is equal to the
    /// initial simulation time incremented by the specified duration, whether
    /// or not an event was scheduled for that time.
    pub fn step_by(&mut self, duration: Duration) -> Result<(), ExecutionError> {
        let target_time = self.time.read() + duration;

        self.step_until_unchecked(target_time)
    }

    /// Iteratively advances the simulation time until the specified deadline,
    /// as if by calling [`Simulation::step()`] repeatedly.
    ///
    /// This method blocks until all events scheduled up to the specified target
    /// time have completed. The simulation time upon completion is equal to the
    /// specified target time, whether or not an event was scheduled for that
    /// time.
    pub fn step_until(&mut self, target_time: MonotonicTime) -> Result<(), ExecutionError> {
        if self.time.read() >= target_time {
            return Err(ExecutionError::InvalidTargetTime(target_time));
        }
        self.step_until_unchecked(target_time)
    }

    /// Returns an owned scheduler handle.
    pub fn scheduler(&self) -> Scheduler {
        Scheduler::new(self.scheduler_queue.clone(), self.time.reader())
    }

    /// Processes an action immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged. The periodicity of the action, if
    /// any, is ignored.
    pub fn process(&mut self, action: Action) -> Result<(), ExecutionError> {
        action.spawn_and_forget(&self.executor);
        self.run()
    }

    /// Processes an event immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged.
    pub fn process_event<M, F, T, S>(
        &mut self,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<(), ExecutionError>
    where
        M: Model,
        F: for<'a> InputFn<'a, M, T, S>,
        T: Send + Clone + 'static,
    {
        let sender = address.into().0;
        let fut = async move {
            // Ignore send errors.
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

        self.executor.spawn_and_forget(fut);
        self.run()
    }

    /// Processes a query immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged. If the targeted model was not added
    /// to the simulation, an `ExecutionError::InvalidQuery` is returned.
    pub fn process_query<M, F, T, R, S>(
        &mut self,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<R, ExecutionError>
    where
        M: Model,
        F: for<'a> ReplierFn<'a, M, T, R, S>,
        T: Send + Clone + 'static,
        R: Send + 'static,
    {
        let (reply_writer, mut reply_reader) = slot::slot();
        let sender = address.into().0;

        let fut = async move {
            // Ignore send errors.
            let _ = sender
                .send(
                    move |model: &mut M,
                          scheduler,
                          recycle_box: RecycleBox<()>|
                          -> RecycleBox<dyn Future<Output = ()> + Send + '_> {
                        let fut = async move {
                            let reply = func.call(model, arg, scheduler).await;
                            let _ = reply_writer.write(reply);
                        };

                        coerce_box!(RecycleBox::recycle(recycle_box, fut))
                    },
                )
                .await;
        };

        self.executor.spawn_and_forget(fut);
        self.run()?;

        reply_reader
            .try_read()
            .map_err(|_| ExecutionError::BadQuery)
    }

    /// Runs the executor.
    fn run(&mut self) -> Result<(), ExecutionError> {
        if self.is_terminated {
            return Err(ExecutionError::Terminated);
        }

        self.executor.run(self.timeout).map_err(|e| match e {
            ExecutorError::Deadlock => {
                self.is_terminated = true;
                let mut deadlock_info = Vec::new();
                for (name, observer) in &self.observers {
                    let mailbox_size = observer.len();
                    if mailbox_size != 0 {
                        deadlock_info.push(DeadlockInfo {
                            model_name: name.clone(),
                            mailbox_size,
                        });
                    }
                }

                ExecutionError::Deadlock(deadlock_info)
            }
            ExecutorError::Timeout => {
                self.is_terminated = true;

                ExecutionError::Timeout
            }
        })
    }

    /// Advances simulation time to that of the next scheduled action if its
    /// scheduling time does not exceed the specified bound, processing that
    /// action as well as all other actions scheduled for the same time.
    ///
    /// If at least one action was found that satisfied the time bound, the
    /// corresponding new simulation time is returned.
    fn step_to_next_bounded(
        &mut self,
        upper_time_bound: MonotonicTime,
    ) -> Result<Option<MonotonicTime>, ExecutionError> {
        // Function pulling the next action. If the action is periodic, it is
        // immediately re-scheduled.
        fn pull_next_action(scheduler_queue: &mut MutexGuard<SchedulerQueue>) -> Action {
            let ((time, channel_id), action) = scheduler_queue.pull().unwrap();
            if let Some((action_clone, period)) = action.next() {
                scheduler_queue.insert((time + period, channel_id), action_clone);
            }

            action
        }

        // Closure returning the next key which time stamp is no older than the
        // upper bound, if any. Cancelled actions are pulled and discarded.
        let peek_next_key = |scheduler_queue: &mut MutexGuard<SchedulerQueue>| {
            loop {
                match scheduler_queue.peek() {
                    Some((&key, action)) if key.0 <= upper_time_bound => {
                        if !action.is_cancelled() {
                            break Some(key);
                        }
                        // Discard cancelled actions.
                        scheduler_queue.pull();
                    }
                    _ => break None,
                }
            }
        };

        // Move to the next scheduled time.
        let mut scheduler_queue = self.scheduler_queue.lock().unwrap();
        let mut current_key = match peek_next_key(&mut scheduler_queue) {
            Some(key) => key,
            None => return Ok(None),
        };
        self.time.write(current_key.0);

        loop {
            let action = pull_next_action(&mut scheduler_queue);
            let mut next_key = peek_next_key(&mut scheduler_queue);
            if next_key != Some(current_key) {
                // Since there are no other actions targeting the same mailbox
                // and the same time, the action is spawned immediately.
                action.spawn_and_forget(&self.executor);
            } else {
                // To ensure that their relative order of execution is
                // preserved, all actions targeting the same mailbox are
                // executed sequentially within a single compound future.
                let mut action_sequence = SeqFuture::new();
                action_sequence.push(action.into_future());
                loop {
                    let action = pull_next_action(&mut scheduler_queue);
                    action_sequence.push(action.into_future());
                    next_key = peek_next_key(&mut scheduler_queue);
                    if next_key != Some(current_key) {
                        break;
                    }
                }

                // Spawn a compound future that sequentially polls all actions
                // targeting the same mailbox.
                self.executor.spawn_and_forget(action_sequence);
            }

            current_key = match next_key {
                // If the next action is scheduled at the same time, update the
                // key and continue.
                Some(k) if k.0 == current_key.0 => k,
                // Otherwise wait until all actions have completed and return.
                _ => {
                    drop(scheduler_queue); // make sure the queue's mutex is released.

                    let current_time = current_key.0;
                    if let SyncStatus::OutOfSync(lag) = self.clock.synchronize(current_time) {
                        if let Some(tolerance) = &self.clock_tolerance {
                            if &lag > tolerance {
                                return Err(ExecutionError::OutOfSync(lag));
                            }
                        }
                    }
                    self.run()?;

                    return Ok(Some(current_time));
                }
            };
        }
    }

    /// Iteratively advances simulation time and processes all actions scheduled
    /// up to the specified target time.
    ///
    /// Once the method returns it is guaranteed that (i) all actions scheduled
    /// up to the specified target time have completed and (ii) the final
    /// simulation time matches the target time.
    ///
    /// This method does not check whether the specified time lies in the future
    /// of the current simulation time.
    fn step_until_unchecked(&mut self, target_time: MonotonicTime) -> Result<(), ExecutionError> {
        loop {
            match self.step_to_next_bounded(target_time) {
                // The target time was reached exactly.
                Ok(Some(t)) if t == target_time => return Ok(()),
                // No actions are scheduled before or at the target time.
                Ok(None) => {
                    // Update the simulation time.
                    self.time.write(target_time);
                    self.clock.synchronize(target_time);
                    return Ok(());
                }
                Err(e) => return Err(e),
                // The target time was not reached yet.
                _ => {}
            }
        }
    }
}

impl fmt::Debug for Simulation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Simulation")
            .field("time", &self.time.read())
            .finish_non_exhaustive()
    }
}

/// Information regarding a deadlocked model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeadlockInfo {
    /// Name of the deadlocked model.
    pub model_name: String,
    /// Number of messages in the mailbox.
    pub mailbox_size: usize,
}

/// An error returned upon simulation execution failure.
///
/// Note that if a `Deadlock`, `ModelError` or `ModelPanic` is returned, any
/// subsequent attempt to run the simulation will return `Terminated`.
#[derive(Debug)]
pub enum ExecutionError {
    /// The simulation has deadlocked.
    ///
    /// Enlists all models with non-empty mailboxes.
    Deadlock(Vec<DeadlockInfo>),
    /// A model has aborted the simulation.
    ModelError {
        /// Name of the model.
        model_name: String,
        /// Error registered by the model.
        error: Box<dyn Error>,
    },
    /// A panic was caught during execution with the message contained in the
    /// payload.
    Panic(String),
    /// The simulation step has failed to complete within the allocated time.
    Timeout,
    /// The simulation has lost synchronization with the clock and lags behind
    /// by the duration given in the payload.
    OutOfSync(Duration),
    /// The specified target simulation time is in the past of the current
    /// simulation time.
    InvalidTargetTime(MonotonicTime),
    /// The query was invalid and did not obtain a response.
    BadQuery,
    /// The simulation has been terminated due to an earlier deadlock, model
    /// error, model panic or timeout.
    Terminated,
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Deadlock(list) => {
                f.write_str(
                    "a simulation deadlock has been detected that involves the following models: ",
                )?;
                let mut first_item = true;
                for info in list {
                    if first_item {
                        first_item = false;
                    } else {
                        f.write_str(", ")?;
                    }
                    write!(
                        f,
                        "'{}' ({} item{} in mailbox)",
                        info.model_name,
                        info.mailbox_size,
                        if info.mailbox_size == 1 { "" } else { "s" }
                    )?;
                }

                Ok(())
            }
            Self::ModelError { model_name, error } => {
                write!(
                    f,
                    "the simulation has been aborted by model '{}' with the following error: {}",
                    model_name, error
                )
            }
            Self::Panic(msg) => {
                f.write_str("a panic has been caught during simulation:\n")?;
                f.write_str(msg)
            }
            Self::Timeout => f.write_str("the simulation step has failed to complete within the allocated time"),
            Self::OutOfSync(lag) => {
                write!(
                    f,
                    "the simulation has lost synchronization and lags behind the clock by '{:?}'",
                    lag
                )
            }
            Self::InvalidTargetTime(time) => {
                write!(
                    f,
                    "target simulation stamp {} lies in the past of the current simulation time",
                    time
                )
            }
            Self::BadQuery => f.write_str("the query did not return any response; maybe the target model was not added to the simulation?"),
            Self::Terminated => f.write_str("the simulation has been terminated"),
        }
    }
}

impl Error for ExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::ModelError { error, .. } = &self {
            Some(error.as_ref())
        } else {
            None
        }
    }
}

/// An error returned upon simulation execution or scheduling failure.
#[derive(Debug)]
pub enum SimulationError {
    /// The execution of the simulation failed.
    ExecutionError(ExecutionError),
    /// An attempt to schedule an item failed.
    SchedulingError(SchedulingError),
}

impl fmt::Display for SimulationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ExecutionError(e) => e.fmt(f),
            Self::SchedulingError(e) => e.fmt(f),
        }
    }
}

impl Error for SimulationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecutionError(e) => e.source(),
            Self::SchedulingError(e) => e.source(),
        }
    }
}

impl From<ExecutionError> for SimulationError {
    fn from(e: ExecutionError) -> Self {
        Self::ExecutionError(e)
    }
}

impl From<SchedulingError> for SimulationError {
    fn from(e: SchedulingError) -> Self {
        Self::SchedulingError(e)
    }
}

/// Adds a model and its mailbox to the simulation bench.
pub(crate) fn add_model<P: ProtoModel>(
    model: P,
    mailbox: Mailbox<P::Model>,
    name: String,
    scheduler: Scheduler,
    executor: &Executor,
    abort_signal: &Signal,
) {
    #[cfg(feature = "tracing")]
    let span = tracing::span!(target: env!("CARGO_PKG_NAME"), tracing::Level::INFO, "model", name);

    let context = Context::new(name, LocalScheduler::new(scheduler, mailbox.address()));
    let build_context = BuildContext::new(&mailbox, &context, executor, abort_signal);

    let model = model.build(&build_context);

    let mut receiver = mailbox.0;
    let abort_signal = abort_signal.clone();

    let fut = async move {
        let mut model = model.init(&context).await.0;
        while !abort_signal.is_set() && receiver.recv(&mut model, &context).await.is_ok() {}
    };

    #[cfg(feature = "tracing")]
    let fut = tracing::Instrument::instrument(fut, span);

    executor.spawn_and_forget(fut);
}
