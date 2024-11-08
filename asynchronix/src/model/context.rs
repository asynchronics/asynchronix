use std::fmt;

use crate::executor::{Executor, Signal};
use crate::simulation::{self, LocalScheduler, Mailbox};

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
///         ctx: &BuildContext<Self>)
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
///         ctx.add_submodel(submult1, submult1_mbox, "submultiplier 1");
///         ctx.add_submodel(submult2, submult2_mbox, "submultiplier 2");
///
///         mult
///     }
/// }
///
/// ```
#[derive(Debug)]
pub struct BuildContext<'a, P: ProtoModel> {
    /// Mailbox of the model.
    pub mailbox: &'a Mailbox<P::Model>,
    context: &'a Context<P::Model>,
    executor: &'a Executor,
    abort_signal: &'a Signal,
}

impl<'a, P: ProtoModel> BuildContext<'a, P> {
    /// Creates a new local context.
    pub(crate) fn new(
        mailbox: &'a Mailbox<P::Model>,
        context: &'a Context<P::Model>,
        executor: &'a Executor,
        abort_signal: &'a Signal,
    ) -> Self {
        Self {
            mailbox,
            context,
            executor,
            abort_signal,
        }
    }

    /// Returns the model instance name.
    pub fn name(&self) -> &str {
        &self.context.name
    }

    /// Adds a sub-model to the simulation bench.
    ///
    /// The `name` argument needs not be unique. If an empty string is provided,
    /// it is replaced by the string `<unknown>`.
    ///
    /// The provided name is appended to that of the parent model using a dot as
    /// a separator (e.g. `parent_name.child_name`) to build an identifier. This
    /// identifier is used for logging or error-reporting purposes.
    pub fn add_submodel<S: ProtoModel>(
        &self,
        model: S,
        mailbox: Mailbox<S::Model>,
        name: impl Into<String>,
    ) {
        let mut submodel_name = name.into();
        if submodel_name.is_empty() {
            submodel_name = String::from("<unknown>");
        };
        submodel_name = self.context.name().to_string() + "." + &submodel_name;

        simulation::add_model(
            model,
            mailbox,
            submodel_name,
            self.context.scheduler.scheduler.clone(),
            self.executor,
            self.abort_signal,
        );
    }
}
