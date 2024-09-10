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
//! # let mut simu = SimInit::new().init(MonotonicTime::EPOCH);
//! simu.process_event(
//!     |m: &mut ModelA| {
//!         m.output.connect(ModelB::input, modelB_addr);
//!     },
//!     (),
//!     &modelA_addr
//! );
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

use crate::executor::Executor;
use crate::model::{Context, Model, SetupContext};
use crate::ports::{InputFn, ReplierFn};
use crate::time::{Clock, MonotonicTime, TearableAtomicTime};
use crate::util::seq_futures::SeqFuture;
use crate::util::slot;
use crate::util::sync_cell::SyncCell;

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
    time: SyncCell<TearableAtomicTime>,
    clock: Box<dyn Clock>,
}

impl Simulation {
    /// Creates a new `Simulation` with the specified clock.
    pub(crate) fn new(
        executor: Executor,
        scheduler_queue: Arc<Mutex<SchedulerQueue>>,
        time: SyncCell<TearableAtomicTime>,
        clock: Box<dyn Clock + 'static>,
    ) -> Self {
        Self {
            executor,
            scheduler_queue,
            time,
            clock,
        }
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
    pub fn step(&mut self) {
        self.step_to_next_bounded(MonotonicTime::MAX);
    }

    /// Iteratively advances the simulation time by the specified duration, as
    /// if by calling [`Simulation::step()`] repeatedly.
    ///
    /// This method blocks until all events scheduled up to the specified target
    /// time have completed. The simulation time upon completion is equal to the
    /// initial simulation time incremented by the specified duration, whether
    /// or not an event was scheduled for that time.
    pub fn step_by(&mut self, duration: Duration) {
        let target_time = self.time.read() + duration;

        self.step_until_unchecked(target_time);
    }

    /// Iteratively advances the simulation time until the specified deadline,
    /// as if by calling [`Simulation::step()`] repeatedly.
    ///
    /// This method blocks until all events scheduled up to the specified target
    /// time have completed. The simulation time upon completion is equal to the
    /// specified target time, whether or not an event was scheduled for that
    /// time.
    pub fn step_until(&mut self, target_time: MonotonicTime) -> Result<(), SchedulingError> {
        if self.time.read() >= target_time {
            return Err(SchedulingError::InvalidScheduledTime);
        }
        self.step_until_unchecked(target_time);

        Ok(())
    }

    /// Returns a scheduler handle.
    pub fn scheduler(&self) -> Scheduler {
        Scheduler::new(self.scheduler_queue.clone(), self.time.reader())
    }

    /// Processes an action immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged. The periodicity of the action, if
    /// any, is ignored.
    pub fn process(&mut self, action: Action) {
        action.spawn_and_forget(&self.executor);
        self.executor.run();
    }

    /// Processes an event immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged.
    pub fn process_event<M, F, T, S>(&mut self, func: F, arg: T, address: impl Into<Address<M>>)
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
        self.executor.run();
    }

    /// Processes a query immediately, blocking until completion.
    ///
    /// Simulation time remains unchanged.
    pub fn process_query<M, F, T, R, S>(
        &mut self,
        func: F,
        arg: T,
        address: impl Into<Address<M>>,
    ) -> Result<R, QueryError>
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
        self.executor.run();

        reply_reader.try_read().map_err(|_| QueryError {})
    }

    /// Advances simulation time to that of the next scheduled action if its
    /// scheduling time does not exceed the specified bound, processing that
    /// action as well as all other actions scheduled for the same time.
    ///
    /// If at least one action was found that satisfied the time bound, the
    /// corresponding new simulation time is returned.
    fn step_to_next_bounded(&mut self, upper_time_bound: MonotonicTime) -> Option<MonotonicTime> {
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
        let mut current_key = peek_next_key(&mut scheduler_queue)?;
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
                    // TODO: check synchronization status?
                    self.clock.synchronize(current_time);
                    self.executor.run();

                    return Some(current_time);
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
    fn step_until_unchecked(&mut self, target_time: MonotonicTime) {
        loop {
            match self.step_to_next_bounded(target_time) {
                // The target time was reached exactly.
                Some(t) if t == target_time => return,
                // No actions are scheduled before or at the target time.
                None => {
                    // Update the simulation time.
                    self.time.write(target_time);
                    self.clock.synchronize(target_time);
                    return;
                }
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

/// Error returned when a query did not obtain a response.
///
/// This can happen either because the model targeted by the address was not
/// added to the simulation or due to a simulation deadlock.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct QueryError {}

impl fmt::Display for QueryError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "the query did not receive a response")
    }
}

impl Error for QueryError {}

/// Adds a model and its mailbox to the simulation bench.
pub(crate) fn add_model<M: Model>(
    mut model: M,
    mailbox: Mailbox<M>,
    name: String,
    scheduler: Scheduler,
    executor: &Executor,
) {
    #[cfg(feature = "tracing")]
    let span = tracing::span!(target: env!("CARGO_PKG_NAME"), tracing::Level::INFO, "model", name);

    let context = Context::new(name, LocalScheduler::new(scheduler, mailbox.address()));
    let setup_context = SetupContext::new(&mailbox, &context, executor);

    model.setup(&setup_context);

    let mut receiver = mailbox.0;
    let fut = async move {
        let mut model = model.init(&context).await.0;
        while receiver.recv(&mut model, &context).await.is_ok() {}
    };

    #[cfg(feature = "tracing")]
    let fut = tracing::Instrument::instrument(fut, span);

    executor.spawn_and_forget(fut);
}
