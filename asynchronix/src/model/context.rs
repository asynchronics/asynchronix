use std::fmt;

use crate::executor::Executor;
use crate::simulation::{self, LocalScheduler, Mailbox};

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
///         let time = context.scheduler.time();
///         let greeting = format!("Hello, this message was scheduled at: {:?}.", time);
///
///         if delay.is_zero() {
///             self.msg_out.send(greeting).await;
///         } else {
///             context.scheduler.schedule_event(delay, Self::send_msg, greeting).unwrap();
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

    /// Local scheduler.
    pub scheduler: LocalScheduler<M>,
}

impl<M: Model> Context<M> {
    /// Creates a new local context.
    pub(crate) fn new(name: String, scheduler: LocalScheduler<M>) -> Self {
        Self { name, scheduler }
    }

    /// Returns the model instance name.
    pub fn name(&self) -> &str {
        &self.name
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
///            let a_name = setup_context.name().to_string() + "::a";
///            let b_name = setup_context.name().to_string() + "::b";
///
///            a.out.connect(SubmodelB::input, &b_mbox);
///
///            setup_context.add_model(a, a_mbox, a_name);
///            setup_context.add_model(b, b_mbox, b_name);
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

    /// Returns the model instance name.
    pub fn name(&self) -> &str {
        &self.context.name
    }

    /// Adds a new model and its mailbox to the simulation bench.
    ///
    /// The `name` argument needs not be unique (it can be an empty string) and
    /// is used for convenience for model instance identification (e.g. for
    /// logging purposes).
    pub fn add_model<N: Model>(&self, model: N, mailbox: Mailbox<N>, name: impl Into<String>) {
        let mut submodel_name = name.into();
        if !self.context.name().is_empty() && !submodel_name.is_empty() {
            submodel_name = self.context.name().to_string() + "." + &submodel_name;
        }
        simulation::add_model(
            model,
            mailbox,
            submodel_name,
            self.context.scheduler.scheduler.clone(),
            self.executor,
        );
    }
}
