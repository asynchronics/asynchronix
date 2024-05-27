//! Model components.
//!
//! # Model trait
//!
//! Every model must implement the [`Model`] trait. This trait defines
//! * a setup method, [`Model::setup()`], which main purpose is to create,
//!   connect and add to the simulation bench submodels and perform other setup
//!   steps,
//! * an asynchronous initialization method, [`Model::init()`], which main
//!   purpose is to enable models to perform specific actions only once all
//!   models have been connected and migrated to the simulation, but before the
//!   simulation actually starts.
//!
//! #### Examples
//!
//! A model that does not require setup and initialization can simply use the
//! default implementation of the `Model` trait:
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
//! Otherwise, custom `setup()` or `init()` methods can be implemented:
//!
//! ```
//! use std::future::Future;
//! use std::pin::Pin;
//!
//! use asynchronix::model::{Context, InitializedModel, Model, SetupContext};
//!
//! pub struct MyModel {
//!     // ...
//! }
//! impl Model for MyModel {
//!     fn setup(
//!         &mut self,
//!         setup_context: &SetupContext<Self>) {
//!             println!("...setup...");
//!     }
//!
//!     async fn init(
//!         mut self,
//!         context: &Context<Self>
//!     ) -> InitializedModel<Self> {
//!         println!("...initialization...");
//!
//!         self.into()
//!     }
//! }
//! ```
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

pub use context::{Context, SetupContext};

mod context;

/// Trait to be implemented by all models.
///
/// This trait enables models to perform specific actions during setup and
/// initialization. The [`Model::setup()`] method is run only once when models
/// are being added to the simulation bench. This method allows in particular
/// sub-models to be created, connected and added to the simulation.
///
/// The [`Model::init()`] method is run only once all models have been connected and
/// migrated to the simulation bench, but before the simulation actually starts.
/// A common use for `init` is to send messages to connected models at the
/// beginning of the simulation.
///
/// The `init` function converts the model to the opaque `InitializedModel` type
/// to prevent an already initialized model from being added to the simulation
/// bench.
pub trait Model: Sized + Send + 'static {
    /// Performs model setup.
    ///
    /// This method is executed exactly once for all models of the simulation
    /// when the [`SimInit::add_model()`](crate::simulation::SimInit::add_model)
    /// method is called.
    ///
    /// The default implementation does nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    /// use std::pin::Pin;
    ///
    /// use asynchronix::model::{InitializedModel, Model, SetupContext};
    ///
    /// pub struct MyModel {
    ///     // ...
    /// }
    ///
    /// impl Model for MyModel {
    ///     fn setup(
    ///         &mut self,
    ///         setup_context: &SetupContext<Self>
    ///     ) {
    ///         println!("...setup...");
    ///     }
    /// }
    /// ```
    fn setup(&mut self, _: &SetupContext<Self>) {}

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
