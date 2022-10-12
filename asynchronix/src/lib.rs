//! Asynchronix: a high-performance asynchronous computation framework for
//! system simulation.

#![warn(missing_docs, missing_debug_implementations, unreachable_pub)]

mod loom_exports;
pub(crate) mod macros;
pub mod runtime;

#[cfg(feature = "dev-hooks")]
pub mod dev_hooks;
