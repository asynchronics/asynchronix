//! Model ports for event and query broadcasting.
//!
//! Models typically contain [`Output`] and/or [`Requestor`] ports, exposed as
//! public member variables. Output ports broadcast events to all connected
//! input ports, while requestor ports broadcast queries to, and retrieve
//! replies from, all connected replier ports.
//!
//! On the surface, output and requestor ports only differ in that sending a
//! query from a requestor port also returns an iterator over the replies from
//! all connected ports. Sending a query is more costly, however, because of the
//! need to wait until all connected models have processed the query. In
//! contrast, since events are buffered in the mailbox of the target model,
//! sending an event is a fire-and-forget operation. For this reason, output
//! ports should generally be preferred over requestor ports when possible.
//!
//! `Output` and `Requestor` ports are clonable. Their clones are shallow
//! copies, meaning that any modification of the ports connected to one clone is
//! immediately reflected in other clones.
//!
//! #### Example
//!
//! This example demonstrates two submodels inside a parent model. The output of
//! the submodel and of the main model are clones and remain therefore always
//! connected to the same inputs.
//!
//! For a more comprehensive example demonstrating hierarchical model
//! assemblies, see the [`assembly example`][assembly].
//!
//! [assembly]:
//!     https://github.com/asynchronics/asynchronix/tree/main/asynchronix/examples/assembly.rs
//!
//! ```
//! use asynchronix::model::{BuildContext, Model, ProtoModel};
//! use asynchronix::ports::Output;
//! use asynchronix::simulation::Mailbox;
//!
//! pub struct ChildModel {
//!     pub output: Output<u64>,
//! }
//!
//! impl ChildModel {
//!     pub fn new(output: Output<u64>) -> Self {
//!         Self {
//!             output,
//!         }
//!     }
//! }
//!
//! impl Model for ChildModel {}
//!
//! pub struct ParentModel {
//!     output: Output<u64>,
//! }
//!
//! impl Model for ParentModel {}
//!
//! pub struct ProtoParentModel {
//!     pub output: Output<u64>,
//! }
//!
//! impl ProtoParentModel {
//!     pub fn new() -> Self {
//!         Self {
//!             output: Default::default(),
//!         }
//!     }
//! }
//!
//! impl ProtoModel for ProtoParentModel {
//!     type Model = ParentModel;
//!
//!     fn build(self, ctx: &mut BuildContext<Self>) -> ParentModel {
//!         let mut child = ChildModel::new(self.output.clone());
//!
//!         ctx.add_submodel(child, Mailbox::new(), "child");
//!
//!         ParentModel { output: self.output }
//!     }
//! }
//! ```

mod input;
mod output;
mod sink;
mod source;

pub use input::markers;
pub use input::{InputFn, ReplierFn};
pub use output::{Output, Requestor};
pub use sink::{
    event_buffer::EventBuffer, event_slot::EventSlot, EventSink, EventSinkStream, EventSinkWriter,
};
pub use source::{EventSource, QuerySource, ReplyReceiver};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
/// Unique identifier for a connection between two ports.
pub struct LineId(u64);

/// Error raised when the specified line cannot be found.
#[derive(Copy, Clone, Debug)]
pub struct LineError {}
