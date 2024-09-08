//! A high-performance, discrete-event computation framework for system
//! simulation.
//!
//! Asynchronix is a developer-friendly, yet highly optimized software simulator
//! able to scale to very large simulation with complex time-driven state
//! machines.
//!
//! It promotes a component-oriented architecture that is familiar to system
//! engineers and closely resembles [flow-based programming][FBP]: a model is
//! essentially an isolated entity with a fixed set of typed inputs and outputs,
//! communicating with other models through message passing via connections
//! defined during bench assembly. Unlike in conventional flow-based
//! programming, request-reply patterns are also possible.
//!
//! Asynchronix leverages asynchronous programming to perform
//! auto-parallelization in a manner that is fully transparent to model authors
//! and users, achieving high computational throughput on large simulation
//! benches by means of a custom multi-threaded executor.
//!
//!
//! [FBP]: https://en.wikipedia.org/wiki/Flow-based_programming
//!
//! # A practical overview
//!
//! Simulating a system typically involves three distinct activities:
//!
//! 1. the design of simulation models for each sub-system,
//! 2. the assembly of a simulation bench from a set of models, performed by
//!    inter-connecting model ports,
//! 3. the execution of the simulation, managed through periodical increments of
//!    the simulation time and by exchange of messages with simulation models.
//!
//! The following sections go through each of these activities in more details.
//!
//! ## Authoring models
//!
//! Models can contain four kinds of ports:
//!
//! * _output ports_, which are instances of the [`Output`](ports::Output) type
//!   and can be used to broadcast a message,
//! * _requestor ports_, which are instances of the
//!   [`Requestor`](ports::Requestor) type and can be used to broadcast a
//!   message and receive an iterator yielding the replies from all connected
//!   replier ports,
//! * _input ports_, which are synchronous or asynchronous methods that
//!   implement the [`InputFn`](ports::InputFn) trait and take an `&mut self`
//!   argument, a message argument, and an optional
//!   [`&Context`](model::Context) argument,
//! * _replier ports_, which are similar to input ports but implement the
//!   [`ReplierFn`](ports::ReplierFn) trait and return a reply.
//!
//! Messages that are broadcast by an output port to an input port are referred
//! to as *events*, while messages exchanged between requestor and replier ports
//! are referred to as *requests* and *replies*.
//!
//! Models must implement the [`Model`](model::Model) trait. The main purpose of
//! this trait is to allow models to specify
//! * a `setup()` method that is called once during model addtion to simulation,
//!   this method allows e.g. creation and interconnection of submodels inside
//!   the model,
//! * an `init()` method that is guaranteed to run once and only once when the
//!   simulation is initialized, _i.e._ after all models have been connected but
//!   before the simulation starts.
//!
//! The `setup()` and `init()` methods have default implementations, so models
//! that do not require setup and initialization can simply implement the trait
//! with a one-liner such as `impl Model for MyModel {}`.
//!
//! #### A simple model
//!
//! Let us consider for illustration a simple model that forwards its input
//! after multiplying it by 2. This model has only one input and one output
//! port:
//!
//! ```text
//!                ┌────────────┐
//!                │            │
//! Input ●───────▶│ Multiplier ├───────▶ Output
//!          f64   │            │  f64
//!                └────────────┘
//! ```
//!
//! `Multiplier` could be implemented as follows:
//!
//! ```
//! use asynchronix::model::Model;
//! use asynchronix::ports::Output;
//!
//! #[derive(Default)]
//! pub struct Multiplier {
//!     pub output: Output<f64>,
//! }
//! impl Multiplier {
//!     pub async fn input(&mut self, value: f64) {
//!         self.output.send(2.0 * value).await;
//!     }
//! }
//! impl Model for Multiplier {}
//! ```
//!
//! #### A model using the local context
//!
//! Models frequently need to schedule actions at a future time or simply get
//! access to the current simulation time. To do so, input and replier methods
//! can take an optional argument that gives them access to a local context.
//!
//! To show how the local context can be used in practice, let us implement
//! `Delay`, a model which simply forwards its input unmodified after a 1s
//! delay:
//!
//! ```
//! use std::time::Duration;
//! use asynchronix::model::{Context, Model};
//! use asynchronix::ports::Output;
//!
//! #[derive(Default)]
//! pub struct Delay {
//!    pub output: Output<f64>,
//! }
//! impl Delay {
//!     pub fn input(&mut self, value: f64, context: &Context<Self>) {
//!         context.scheduler.schedule_event(Duration::from_secs(1), Self::send, value).unwrap();
//!     }
//!
//!     async fn send(&mut self, value: f64) {
//!         self.output.send(value).await;
//!     }
//! }
//! impl Model for Delay {}
//! ```
//!
//! ## Assembling simulation benches
//!
//! A simulation bench is a system of inter-connected models that have been
//! migrated to a simulation.
//!
//! The assembly process usually starts with the instantiation of models and the
//! creation of a [`Mailbox`](simulation::Mailbox) for each model. A mailbox is
//! essentially a fixed-capacity buffer for events and requests. While each
//! model has only one mailbox, it is possible to create an arbitrary number of
//! [`Address`](simulation::Mailbox)es pointing to that mailbox.
//!
//! Addresses are used among others to connect models: each output or requestor
//! port has a `connect()` method that takes as argument a function pointer to
//! the corresponding input or replier port method and the address of the
//! targeted model.
//!
//! Once all models are connected, they are added to a
//! [`SimInit`](simulation::SimInit) instance, which is a builder type for the
//! final [`Simulation`](simulation::Simulation).
//!
//! The easiest way to understand the assembly step is with a short example. Say
//! that we want to assemble the following system from the models implemented
//! above:
//!
//! ```text
//!                                ┌────────────┐
//!                                │            │
//!                            ┌──▶│   Delay    ├──┐
//!           ┌────────────┐   │   │            │  │   ┌────────────┐
//!           │            │   │   └────────────┘  │   │            │
//! Input ●──▶│ Multiplier ├───┤                   ├──▶│   Delay    ├──▶ Output
//!           │            │   │   ┌────────────┐  │   │            │
//!           └────────────┘   │   │            │  │   └────────────┘
//!                            └──▶│ Multiplier ├──┘
//!                                │            │
//!                                └────────────┘
//! ```
//!
//! Here is how this could be done:
//!
//! ```
//! # mod models {
//! #     use std::time::Duration;
//! #     use asynchronix::model::{Context, Model};
//! #     use asynchronix::ports::Output;
//! #     #[derive(Default)]
//! #     pub struct Multiplier {
//! #         pub output: Output<f64>,
//! #     }
//! #     impl Multiplier {
//! #         pub async fn input(&mut self, value: f64) {
//! #             self.output.send(2.0 * value).await;
//! #         }
//! #     }
//! #     impl Model for Multiplier {}
//! #     #[derive(Default)]
//! #     pub struct Delay {
//! #        pub output: Output<f64>,
//! #     }
//! #     impl Delay {
//! #         pub fn input(&mut self, value: f64, context: &Context<Self>) {
//! #             context.scheduler.schedule_event(Duration::from_secs(1), Self::send, value).unwrap();
//! #         }
//! #         async fn send(&mut self, value: f64) { // this method can be private
//! #             self.output.send(value).await;
//! #         }
//! #     }
//! #     impl Model for Delay {}
//! # }
//! use std::time::Duration;
//! use asynchronix::ports::EventSlot;
//! use asynchronix::simulation::{Mailbox, SimInit};
//! use asynchronix::time::MonotonicTime;
//!
//! use models::{Delay, Multiplier};
//!
//! // Instantiate models.
//! let mut multiplier1 = Multiplier::default();
//! let mut multiplier2 = Multiplier::default();
//! let mut delay1 = Delay::default();
//! let mut delay2 = Delay::default();
//!
//! // Instantiate mailboxes.
//! let multiplier1_mbox = Mailbox::new();
//! let multiplier2_mbox = Mailbox::new();
//! let delay1_mbox = Mailbox::new();
//! let delay2_mbox = Mailbox::new();
//!
//! // Connect the models.
//! multiplier1.output.connect(Delay::input, &delay1_mbox);
//! multiplier1.output.connect(Multiplier::input, &multiplier2_mbox);
//! multiplier2.output.connect(Delay::input, &delay2_mbox);
//! delay1.output.connect(Delay::input, &delay2_mbox);
//!
//! // Keep handles to the system input and output for the simulation.
//! let mut output_slot = EventSlot::new();
//! delay2.output.connect_sink(&output_slot);
//! let input_address = multiplier1_mbox.address();
//!
//! // Pick an arbitrary simulation start time and build the simulation.
//! let t0 = MonotonicTime::EPOCH;
//! let mut simu = SimInit::new()
//!     .add_model(multiplier1, multiplier1_mbox, "multiplier1")
//!     .add_model(multiplier2, multiplier2_mbox, "multiplier2")
//!     .add_model(delay1, delay1_mbox, "delay1")
//!     .add_model(delay2, delay2_mbox, "delay2")
//!     .init(t0);
//! ```
//!
//! ## Running simulations
//!
//! The simulation can be controlled in several ways:
//!
//! 1. by advancing time, either until the next scheduled event with
//!    [`Simulation::step()`](simulation::Simulation::step), or until a specific
//!    deadline using for instance
//!    [`Simulation::step_by()`](simulation::Simulation::step_by).
//! 2. by sending events or queries without advancing simulation time, using
//!    [`Simulation::process_event()`](simulation::Simulation::process_event) or
//!    [`Simulation::send_query()`](simulation::Simulation::process_query),
//! 3. by scheduling events, using for instance
//!    [`Scheduler::schedule_event()`](simulation::Scheduler::schedule_event).
//!
//! When initialized with the default clock, the simulation will run as fast as
//! possible, without regard for the actual wall clock time. Alternatively, the
//! simulation time can be synchronized to the wall clock time using
//! [`SimInit::set_clock()`](simulation::SimInit::set_clock) and providing a
//! custom [`Clock`](time::Clock) type or a readily-available real-time clock
//! such as [`AutoSystemClock`](time::AutoSystemClock).
//!
//! Simulation outputs can be monitored using [`EventSlot`](ports::EventSlot)s
//! and [`EventBuffer`](ports::EventBuffer)s, which can be connected to any
//! model's output port. While an event slot only gives access to the last value
//! sent from a port, an event stream is an iterator that yields all events that
//! were sent in first-in-first-out order.
//!
//! This is an example of simulation that could be performed using the above
//! bench assembly:
//!
//! ```
//! # mod models {
//! #     use std::time::Duration;
//! #     use asynchronix::model::{Context, Model};
//! #     use asynchronix::ports::Output;
//! #     #[derive(Default)]
//! #     pub struct Multiplier {
//! #         pub output: Output<f64>,
//! #     }
//! #     impl Multiplier {
//! #         pub async fn input(&mut self, value: f64) {
//! #             self.output.send(2.0 * value).await;
//! #         }
//! #     }
//! #     impl Model for Multiplier {}
//! #     #[derive(Default)]
//! #     pub struct Delay {
//! #        pub output: Output<f64>,
//! #     }
//! #     impl Delay {
//! #         pub fn input(&mut self, value: f64, context: &Context<Self>) {
//! #             context.scheduler.schedule_event(Duration::from_secs(1), Self::send, value).unwrap();
//! #         }
//! #         async fn send(&mut self, value: f64) { // this method can be private
//! #             self.output.send(value).await;
//! #         }
//! #     }
//! #     impl Model for Delay {}
//! # }
//! # use std::time::Duration;
//! # use asynchronix::ports::EventSlot;
//! # use asynchronix::simulation::{Mailbox, SimInit};
//! # use asynchronix::time::MonotonicTime;
//! # use models::{Delay, Multiplier};
//! # let mut multiplier1 = Multiplier::default();
//! # let mut multiplier2 = Multiplier::default();
//! # let mut delay1 = Delay::default();
//! # let mut delay2 = Delay::default();
//! # let multiplier1_mbox = Mailbox::new();
//! # let multiplier2_mbox = Mailbox::new();
//! # let delay1_mbox = Mailbox::new();
//! # let delay2_mbox = Mailbox::new();
//! # multiplier1.output.connect(Delay::input, &delay1_mbox);
//! # multiplier1.output.connect(Multiplier::input, &multiplier2_mbox);
//! # multiplier2.output.connect(Delay::input, &delay2_mbox);
//! # delay1.output.connect(Delay::input, &delay2_mbox);
//! # let mut output_slot = EventSlot::new();
//! # delay2.output.connect_sink(&output_slot);
//! # let input_address = multiplier1_mbox.address();
//! # let t0 = MonotonicTime::EPOCH;
//! # let mut simu = SimInit::new()
//! #     .add_model(multiplier1, multiplier1_mbox, "multiplier1")
//! #     .add_model(multiplier2, multiplier2_mbox, "multiplier2")
//! #     .add_model(delay1, delay1_mbox, "delay1")
//! #     .add_model(delay2, delay2_mbox, "delay2")
//! #     .init(t0);
//! // Send a value to the first multiplier.
//! simu.process_event(Multiplier::input, 21.0, &input_address);
//!
//! // The simulation is still at t0 so nothing is expected at the output of the
//! // second delay gate.
//! assert!(output_slot.next().is_none());
//!
//! // Advance simulation time until the next event and check the time and output.
//! simu.step();
//! assert_eq!(simu.time(), t0 + Duration::from_secs(1));
//! assert_eq!(output_slot.next(), Some(84.0));
//!
//! // Get the answer to the ultimate question of life, the universe & everything.
//! simu.step();
//! assert_eq!(simu.time(), t0 + Duration::from_secs(2));
//! assert_eq!(output_slot.next(), Some(42.0));
//! ```
//!
//! # Message ordering guarantees
//!
//! The Asynchronix runtime is based on the [actor model][actor_model], meaning
//! that every simulation model can be thought of as an isolated entity running
//! in its own thread. While in practice the runtime will actually multiplex and
//! migrate models over a fixed set of kernel threads, models will indeed run in
//! parallel whenever possible.
//!
//! Since Asynchronix is a time-based simulator, the runtime will always execute
//! tasks in chronological order, thus eliminating most ordering ambiguities
//! that could result from parallel execution. Nevertheless, it is sometimes
//! possible for events and queries generated in the same time slice to lead to
//! ambiguous execution orders. In order to make it easier to reason about such
//! situations, Asynchronix provides a set of guarantees about message delivery
//! order. Borrowing from the [Pony][pony] programming language, we refer to
//! this contract as *causal messaging*, a property that can be summarized by
//! these two rules:
//!
//! 1. *one-to-one message ordering guarantee*: if model `A` sends two events or
//!    queries `M1` and then `M2` to model `B`, then `B` will always process
//!    `M1` before `M2`,
//! 2. *transitivity guarantee*: if `A` sends `M1` to `B` and then `M2` to `C`
//!    which in turn sends `M3` to `B`, even though `M1` and `M2` may be
//!    processed in any order by `B` and `C`, it is guaranteed that `B` will
//!    process `M1` before `M3`.
//!
//! The first guarantee (and only the first) also extends to events scheduled
//! from a simulation with a
//! [`Scheduler::schedule_*()`](simulation::Scheduler::schedule_event) method:
//! if the scheduler contains several events to be delivered at the same time to
//! the same model, these events will always be processed in the order in which
//! they were scheduled.
//!
//! [actor_model]: https://en.wikipedia.org/wiki/Actor_model
//! [pony]: https://www.ponylang.io/
//!
//!
//! # Other resources
//!
//! ## Other examples
//!
//! The [`examples`][gh_examples] directory in the main repository contains more
//! fleshed out examples that demonstrate various capabilities of the simulation
//! framework.
//!
//! [gh_examples]:
//!     https://github.com/asynchronics/asynchronix/tree/main/asynchronix/examples
//!
//! ## Modules documentation
//!
//! While the above overview does cover the basic concepts, more information is
//! available in the documentation of the different modules:
//!
//! * the [`model`] module provides more details about the signatures of input
//!   and replier port methods and discusses model initialization in the
//!   documentation of [`model::Model`] and self-scheduling methods as well as
//!   scheduling cancellation in the documentation of [`model::Context`],
//! * the [`simulation`] module discusses how the capacity of mailboxes may
//!   affect the simulation, how connections can be modified after the
//!   simulation was instantiated, and which pathological situations can lead to
//!   a deadlock,
//! * the [`time`] module discusses in particular the monotonic timestamp format
//!   used for simulations ([`time::MonotonicTime`]).
#![warn(missing_docs, missing_debug_implementations, unreachable_pub)]
#![cfg_attr(docsrs, feature(doc_auto_cfg, doc_cfg_hide))]
#![cfg_attr(docsrs, doc(cfg_hide(feature = "dev-hooks")))]

pub(crate) mod channel;
pub(crate) mod executor;
#[cfg(feature = "grpc")]
pub mod grpc;
mod loom_exports;
pub(crate) mod macros;
pub mod model;
pub mod ports;
#[cfg(feature = "grpc")]
pub mod registry;
pub mod simulation;
pub mod time;
pub(crate) mod util;

#[cfg(feature = "dev-hooks")]
pub mod dev_hooks;
