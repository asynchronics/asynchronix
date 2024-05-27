//! Simulation management through remote procedure calls.

mod codegen;
mod endpoint_registry;
#[cfg(feature = "grpc-service")]
pub mod grpc;
mod key_registry;
mod simulation_service;
#[cfg(feature = "wasm-service")]
pub mod wasm;

pub use endpoint_registry::EndpointRegistry;
pub use simulation_service::SimulationService;
