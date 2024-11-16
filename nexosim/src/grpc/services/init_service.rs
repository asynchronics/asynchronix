use ciborium;
use serde::de::DeserializeOwned;

use crate::registry::EndpointRegistry;
use crate::simulation::{Scheduler, Simulation, SimulationError};

use super::{map_simulation_error, to_error};

use super::super::codegen::simulation::*;

type InitResult = Result<(Simulation, EndpointRegistry), SimulationError>;
type DeserializationError = ciborium::de::Error<std::io::Error>;
type SimGen = Box<dyn FnMut(&[u8]) -> Result<InitResult, DeserializationError> + Send + 'static>;

/// Protobuf-based simulation initializer.
///
/// An `InitService` creates a new simulation bench based on a serialized
/// initialization configuration.
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
    /// the remote client. It must create a new simulation complemented by a
    /// registry that exposes the public event and query interface.
    pub(crate) fn new<F, I>(mut sim_gen: F) -> Self
    where
        F: FnMut(I) -> Result<(Simulation, EndpointRegistry), SimulationError> + Send + 'static,
        I: DeserializeOwned,
    {
        // Wrap `sim_gen` so it accepts a serialized init configuration.
        let sim_gen = move |serialized_cfg: &[u8]| -> Result<InitResult, DeserializationError> {
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
    ) -> (InitReply, Option<(Simulation, Scheduler, EndpointRegistry)>) {
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
            .and_then(|init_result| init_result.map_err(map_simulation_error));

        let (reply, bench) = match reply {
            Ok((simulation, registry)) => {
                let scheduler = simulation.scheduler();
                (
                    init_reply::Result::Empty(()),
                    Some((simulation, scheduler, registry)),
                )
            }
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
