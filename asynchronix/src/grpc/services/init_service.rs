use ciborium;
use serde::de::DeserializeOwned;

use crate::registry::EndpointRegistry;
use crate::simulation::SimInit;
use crate::simulation::Simulation;

use super::{timestamp_to_monotonic, to_error};

use super::super::codegen::simulation::*;

type DeserializationError = ciborium::de::Error<std::io::Error>;
type SimGen = Box<
    dyn FnMut(&[u8]) -> Result<(SimInit, EndpointRegistry), DeserializationError> + Send + 'static,
>;

/// Protobuf-based simulation initializer.
///
/// An `InitService` creates a new simulation bench based on a serialized initialization configuration.
///
/// It maps the `Init` method defined in `simulation.proto`.
pub(crate) struct InitService {
    sim_gen: SimGen,
}

impl InitService {
    /// Creates a new `InitService`.
    ///
    /// The argument is a closure that takes a CBOR-serialized initialization
    /// configuration and is called every time the simulation is (re)started by
    /// the remote client. It must create a new `SimInit` object complemented by
    /// a registry that exposes the public event and query interface.
    pub(crate) fn new<F, I>(mut sim_gen: F) -> Self
    where
        F: FnMut(I) -> (SimInit, EndpointRegistry) + Send + 'static,
        I: DeserializeOwned,
    {
        // Wrap `sim_gen` so it accepts a serialized init configuration.
        let sim_gen = move |serialized_cfg: &[u8]| -> Result<(SimInit, EndpointRegistry), DeserializationError> {
            let cfg = ciborium::from_reader(serialized_cfg)?;

            Ok(sim_gen(cfg))
        };

        Self {
            sim_gen: Box::new(sim_gen),
        }
    }

    /// Initializes the simulation based on the specified configuration.
    pub(crate) fn init(
        &mut self,
        request: InitRequest,
    ) -> (InitReply, Option<(Simulation, EndpointRegistry)>) {
        let start_time = request.time.unwrap_or_default();

        let reply = (self.sim_gen)(&request.cfg)
            .map_err(|e| {
                to_error(
                    ErrorCode::InvalidMessage,
                    format!(
                        "the initialization configuration could not be deserialized: {}",
                        e
                    ),
                )
            })
            .and_then(|(sim_init, registry)| {
                timestamp_to_monotonic(start_time)
                    .ok_or_else(|| {
                        to_error(ErrorCode::InvalidTime, "out-of-range nanosecond field")
                    })
                    .map(|start_time| (sim_init.init(start_time), registry))
            });

        let (reply, bench) = match reply {
            Ok(bench) => (init_reply::Result::Empty(()), Some(bench)),
            Err(e) => (init_reply::Result::Error(e), None),
        };

        (
            InitReply {
                result: Some(reply),
            },
            bench,
        )
    }
}
