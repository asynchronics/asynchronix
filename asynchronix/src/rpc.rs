//! Simulation management through remote procedure calls.

mod codegen;
mod endpoint_registry;
mod generic_server;
#[cfg(feature = "grpc-server")]
pub mod grpc;
mod key_registry;

pub use endpoint_registry::EndpointRegistry;
