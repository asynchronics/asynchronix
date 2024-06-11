//! Simulation management through remote procedure calls.

mod codegen;
#[cfg(feature = "grpc-service")]
pub mod grpc;
mod key_registry;
mod monitoring_service;
mod simulation_service;
mod sink_registry;
mod source_registry;
#[cfg(feature = "wasm-service")]
pub mod wasm;

pub use monitoring_service::MonitoringService;
pub use simulation_service::SimulationService;
pub use sink_registry::SinkRegistry;
pub use source_registry::SourceRegistry;
