//! WASM simulation service.
//!
//! This module provides [`WasmSimulationService`], a thin wrapper over a
//! [`SimulationService`] that can be use from JavaScript.
//!
//! Although it is readily possible to use a
//! [`Simulation`](crate::simulation::Simulation) object from WASM,
//! [`WasmSimulationService`] goes further by exposing the complete simulation
//! API to JavaScript through protobuf.
//!
//! Keep in mind that WASM only supports single-threaded execution and therefore
//! any simulation bench compiled to WASM should instantiate simulations with
//! either [`SimInit::new()`](crate::simulation::SimInit::new) or
//! [`SimInit::with_num_threads(1)`](crate::simulation::SimInit::with_num_threads),
//! failing which the simulation will panic upon initialization.
//!
//! [`WasmSimulationService`] is exported to the JavaScript namespace as
//! `SimulationService`, and [`WasmSimulationService::process_request`] as
//! `SimulationService.processRequest`.

use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;

use crate::registry::EndpointRegistry;
use crate::simulation::SimInit;

use super::protobuf::ProtobufService;

/// A simulation service that can be used from JavaScript.
///
/// This would typically be used by implementing a `run` function in Rust and
/// export it to WASM:
///
/// ```no_run
/// #[wasm_bindgen]
/// pub fn run() -> WasmSimulationService {
///     WasmSimulationService::new(my_custom_bench_generator)
/// }
/// ```
///
/// which can then be used on the JS side to create a `SimulationService` as a
/// JS object, e.g. with:
///
/// ```js
/// const simu = run();
///
/// // ...build a protobuf request and encode it as a `Uint8Array`...
///
/// const reply = simu.processRequest(myRequest);
///
/// // ...decode the protobuf reply...
/// ```
#[wasm_bindgen(js_name = SimulationService)]
#[derive(Debug)]
pub struct WasmSimulationService(ProtobufService);

#[wasm_bindgen(js_class = SimulationService)]
impl WasmSimulationService {
    /// Processes a protobuf-encoded `AnyRequest` message and returns a
    /// protobuf-encoded reply.
    ///
    /// For the Protocol Buffer definitions, see the `simulation.proto` file.
    #[wasm_bindgen(js_name = processRequest)]
    pub fn process_request(&mut self, request: &[u8]) -> Result<Box<[u8]>, JsError> {
        self.0
            .process_request(request)
            .map(|reply| reply.into_boxed_slice())
            .map_err(|e| JsError::new(&e.to_string()))
    }
}

impl WasmSimulationService {
    /// Creates a new `SimulationService` without any active simulation.
    ///
    /// The argument is a closure that is called every time the simulation is
    /// (re)started by the remote client. It must create a new `SimInit` object
    /// complemented by a registry that exposes the public event and query
    /// interface.
    pub fn new<F, I>(sim_gen: F) -> Self
    where
        F: FnMut(I) -> (SimInit, EndpointRegistry) + Send + 'static,
        I: DeserializeOwned,
    {
        Self(ProtobufService::new(sim_gen))
    }
}
