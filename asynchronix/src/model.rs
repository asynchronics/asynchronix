//! Model components.
//!
//! # Models and model prototypes
//!
//! Every model must implement the [`Model`] trait. This trait defines an
//! asynchronous initialization method, [`Model::init`], which main purpose is
//! to enable models to perform specific actions when the simulation starts,
//! i.e. after all models have been connected and added to the simulation.
//!
//! It is frequently convenient to expose to users a model builder type—called a
//! *model prototype*—rather than the final model. This can be done by
//! implementing the [`ProtoModel`] trait, which defines the associated model
//! type and a [`ProtoModel::build`] method invoked when a model is added the
//! the simulation and returning the actual model instance.
//!
//! Prototype models can be used whenever the Rust builder pattern is helpful,
//! for instance to set optional parameters. One of the use-cases that may
//! benefit from the use of prototype models is hierarchical model building.
//! When a parent model contains submodels, these submodels are often an
//! implementation detail that needs not be exposed to the user. One may then
//! define a prototype model that contains all outputs and requestors ports.
//! Upon invocation of [`ProtoModel::build`], the ports are moved to the
//! appropriate submodels and those submodels are added to the simulation.
//!
//! Note that a trivial [`ProtoModel`] implementation is generated by default
//! for any object implementing the [`Model`] trait, where the associated
//! [`ProtoModel::Model`] type is the model type itself and where
//! [`ProtoModel::build`] simply returns the model instance. This is what makes
//! it possible to use either an explicitly-defined [`ProtoModel`] as argument
//! to the [`SimInit::add_model`](crate::simulation::SimInit::add_model) method,
//! or a plain [`Model`] type.
//!
//! #### Examples
//!
//! A model that does not require initialization or building can simply use the
//! default implementation of the [`Model`] trait:
//!
//! ```
//! use asynchronix::model::Model;
//!
//! pub struct MyModel {
//!     // ...
//! }
//! impl Model for MyModel {}
//! ```
//!
//! If a default action is required during simulation initialization, the `init`
//! methods can be explicitly implemented:
//!
//! ```
//! use asynchronix::model::{Context, InitializedModel, Model};
//!
//! pub struct MyModel {
//!     // ...
//! }
//! impl Model for MyModel {
//!     async fn init(
//!         mut self,
//!         ctx: &Context<Self>
//!     ) -> InitializedModel<Self> {
//!         println!("...initialization...");
//!
//!         self.into()
//!     }
//! }
//! ```
//!
//! Finally, if a model builder is required, the [`ProtoModel`] trait can be
//! explicitly implemented:
//!
//! ```
//! use asynchronix::model::{BuildContext, InitializedModel, Model, ProtoModel};
//! use asynchronix::ports::Output;
//!
//! /// The final model.
//! pub struct Multiplier {
//!     // Private outputs and requestors stored in a form that constitutes an
//!     // implementation detail and should not be exposed to the user.
//!     my_outputs: Vec<Output<usize>>
//! }
//! impl Multiplier {
//!     // Private constructor: the final model is built by the prototype model.
//!     fn new(
//!         value_times_1: Output<usize>,
//!         value_times_2: Output<usize>,
//!         value_times_3: Output<usize>,
//!     ) -> Self {
//!         Self {
//!             my_outputs: vec![value_times_1, value_times_2, value_times_3]
//!         }
//!     }
//!
//!     // Public input to be used during bench construction.
//!     pub async fn my_input(&mut self, my_data: usize) {
//!         for (i, output) in self.my_outputs.iter_mut().enumerate() {
//!             output.send(my_data*(i + 1)).await;
//!         }
//!     }
//! }
//! impl Model for Multiplier {}
//!
//! pub struct ProtoMultiplier {
//!     // Prettyfied outputs exposed to the user.
//!     pub value_times_1: Output<usize>,
//!     pub value_times_2: Output<usize>,
//!     pub value_times_3: Output<usize>,
//! }
//! impl ProtoModel for ProtoMultiplier {
//!     type Model = Multiplier;
//!
//!     fn build(
//!         mut self,
//!         _: &mut BuildContext<Self>
//!     ) -> Multiplier {
//!         Multiplier::new(self.value_times_1, self.value_times_2, self.value_times_3)
//!     }
//! }
//! ```
//!
//!
//! # Events and queries
//!
//! Models can exchange data via *events* and *queries*.
//!
//! Events are send-and-forget messages that can be broadcast from an *output
//! port* to an arbitrary number of *input ports* with a matching event type.
//!
//! Queries actually involve two messages: a *request* that can be broadcast
//! from a *requestor port* to an arbitrary number of *replier ports* with a
//! matching request type, and a *reply* sent in response to such request. The
//! response received by a requestor port is an iterator that yields as many
//! items (replies) as there are connected replier ports.
//!
//!
//! ### Output and requestor ports
//!
//! Output and requestor ports can be added to a model using composition, adding
//! [`Output`](crate::ports::Output) and [`Requestor`](crate::ports::Requestor)
//! objects as members. They are parametrized by the event, request and reply
//! types.
//!
//! Models are expected to expose their output and requestor ports as public
//! members so they can be connected to input and replier ports when assembling
//! the simulation bench.
//!
//! #### Example
//!
//! ```
//! use asynchronix::model::Model;
//! use asynchronix::ports::{Output, Requestor};
//!
//! pub struct MyModel {
//!     pub my_output: Output<String>,
//!     pub my_requestor: Requestor<u32, bool>,
//! }
//! impl MyModel {
//!     // ...
//! }
//! impl Model for MyModel {}
//! ```
//!
//!
//! ### Input and replier ports
//!
//! Input ports and replier ports are methods that implement the
//! [`InputFn`](crate::ports::InputFn) or [`ReplierFn`](crate::ports::ReplierFn)
//! traits with appropriate bounds on their argument and return types.
//!
//! In practice, an input port method for an event of type `T` may have any of
//! the following signatures, where the futures returned by the `async` variants
//! must implement `Send`:
//!
//! ```ignore
//! fn(&mut self) // argument elided, implies `T=()`
//! fn(&mut self, T)
//! fn(&mut self, T, &Context<Self>)
//! async fn(&mut self) // argument elided, implies `T=()`
//! async fn(&mut self, T)
//! async fn(&mut self, T, &Context<Self>)
//! where
//!     Self: Model,
//!     T: Clone + Send + 'static,
//!     R: Send + 'static,
//! ```
//!
//! The context argument is useful for methods that need access to the
//! simulation time or that need to schedule an action at a future date.
//!
//! A replier port for a request of type `T` with a reply of type `R` may in
//! turn have any of the following signatures, where the futures must implement
//! `Send`:
//!
//! ```ignore
//! async fn(&mut self) -> R // argument elided, implies `T=()`
//! async fn(&mut self, T) -> R
//! async fn(&mut self, T, &Context<Self>) -> R
//! where
//!     Self: Model,
//!     T: Clone + Send + 'static,
//!     R: Send + 'static,
//! ```
//!
//! Output and replier ports will normally be exposed as public methods so they
//! can be connected to input and requestor ports when assembling the simulation
//! bench. However, input ports may instead be defined as private methods if
//! they are only used by the model itself to schedule future actions (see the
//! [`Context`] examples).
//!
//! Changing the signature of an input or replier port is not considered to
//! alter the public interface of a model provided that the event, request and
//! reply types remain the same.
//!
//! #### Example
//!
//! ```
//! use asynchronix::model::{Context, Model};
//!
//! pub struct MyModel {
//!     // ...
//! }
//! impl MyModel {
//!     pub fn my_input(&mut self, input: String, context: &Context<Self>) {
//!         // ...
//!     }
//!     pub async fn my_replier(&mut self, request: u32) -> bool { // context argument elided
//!         // ...
//!         # unimplemented!()
//!     }
//! }
//! impl Model for MyModel {}
//! ```
//!

use std::future::Future;

pub use context::{BuildContext, Context};

mod context;

/// Trait to be implemented by simulation models.
///
/// This trait enables models to perform specific actions during initialization.
/// The [`Model::init()`] method is run only once all models have been connected
/// and migrated to the simulation bench, but before the simulation actually
/// starts. A common use for `init` is to send messages to connected models at
/// the beginning of the simulation.
///
/// The `init` function converts the model to the opaque `InitializedModel` type
/// to prevent an already initialized model from being added to the simulation
/// bench.
pub trait Model: Sized + Send + 'static {
    /// Performs asynchronous model initialization.
    ///
    /// This asynchronous method is executed exactly once for all models of the
    /// simulation when the
    /// [`SimInit::init()`](crate::simulation::SimInit::init) method is called.
    ///
    /// The default implementation simply converts the model to an
    /// `InitializedModel` without any side effect.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    /// use std::pin::Pin;
    ///
    /// use asynchronix::model::{Context, InitializedModel, Model};
    ///
    /// pub struct MyModel {
    ///     // ...
    /// }
    ///
    /// impl Model for MyModel {
    ///     async fn init(
    ///         self,
    ///         context: &Context<Self>
    ///     ) -> InitializedModel<Self> {
    ///         println!("...initialization...");
    ///
    ///         self.into()
    ///     }
    /// }
    /// ```
    fn init(self, _: &Context<Self>) -> impl Future<Output = InitializedModel<Self>> + Send {
        async { self.into() }
    }
}

/// Opaque type containing an initialized model.
///
/// A model can be converted to an `InitializedModel` using the `Into`/`From`
/// traits. The implementation of the simulation guarantees that the
/// [`Model::init()`] method will never be called on a model after conversion to
/// an `InitializedModel`.
#[derive(Debug)]
pub struct InitializedModel<M: Model>(pub(crate) M);

impl<M: Model> From<M> for InitializedModel<M> {
    fn from(model: M) -> Self {
        InitializedModel(model)
    }
}

/// Trait to be implemented by model prototypes.
///
/// This trait makes it possible to build the final model from a builder type
/// when it is added to the simulation.
///
/// The [`ProtoModel::build()`] method consumes the prototype. It is
/// automatically called when a model or submodel prototype is added to the
/// simulation using
/// [`Simulation::add_model()`](crate::simulation::SimInit::add_model) or
/// [`BuildContext::add_submodel`].
///
/// The
pub trait ProtoModel: Sized {
    /// Type of the model to be built.
    type Model: Model;

    /// Builds the model.
    ///
    /// This method is invoked when the
    /// [`SimInit::add_model()`](crate::simulation::SimInit::add_model) or
    /// [`BuildContext::add_submodel`] method is called.
    fn build(self, ctx: &mut BuildContext<Self>) -> Self::Model;
}

// Every model can be used as a prototype for itself.
impl<M: Model> ProtoModel for M {
    type Model = Self;

    fn build(self, _: &mut BuildContext<Self>) -> Self::Model {
        self
    }
}
