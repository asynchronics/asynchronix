use std::error;
use std::fmt;

use bytes::Buf;
use prost::Message;

use crate::registry::EndpointRegistry;
use crate::rpc::key_registry::KeyRegistry;
use crate::simulation::SimInit;

use super::codegen::simulation::*;
use super::services::{timestamp_to_monotonic, ControllerService, MonitorService};

/// Protobuf-based simulation manager.
///
/// A `ProtobufService` enables the management of the lifecycle of a
/// simulation, including creating a
/// [`Simulation`](crate::simulation::Simulation), invoking its methods and
/// instantiating a new simulation.
///
/// Its methods map the various RPC service methods defined in
/// `simulation.proto`.
pub(crate) struct ProtobufService {
    sim_gen: Box<dyn FnMut() -> (SimInit, EndpointRegistry) + Send + 'static>,
    controller_service: ControllerService,
    monitor_service: MonitorService,
}

impl ProtobufService {
    /// Creates a new `ProtobufService` without any active simulation.
    ///
    /// The argument is a closure that is called every time the simulation is
    /// (re)started by the remote client. It must create a new `SimInit` object
    /// complemented by a registry that exposes the public event and query
    /// interface.
    pub(crate) fn new<F>(sim_gen: F) -> Self
    where
        F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
    {
        Self {
            sim_gen: Box::new(sim_gen),
            controller_service: ControllerService::NotStarted,
            monitor_service: MonitorService::NotStarted,
        }
    }

    /// Processes an encoded `AnyRequest` message and returns an encoded reply.
    pub(crate) fn process_request<B>(&mut self, request_buf: B) -> Result<Vec<u8>, InvalidRequest>
    where
        B: Buf,
    {
        match AnyRequest::decode(request_buf) {
            Ok(AnyRequest { request: Some(req) }) => match req {
                any_request::Request::InitRequest(request) => {
                    Ok(self.init(request).encode_to_vec())
                }
                any_request::Request::TimeRequest(request) => {
                    Ok(self.controller_service.time(request).encode_to_vec())
                }
                any_request::Request::StepRequest(request) => {
                    Ok(self.controller_service.step(request).encode_to_vec())
                }
                any_request::Request::StepUntilRequest(request) => {
                    Ok(self.controller_service.step_until(request).encode_to_vec())
                }
                any_request::Request::ScheduleEventRequest(request) => Ok(self
                    .controller_service
                    .schedule_event(request)
                    .encode_to_vec()),
                any_request::Request::CancelEventRequest(request) => Ok(self
                    .controller_service
                    .cancel_event(request)
                    .encode_to_vec()),
                any_request::Request::ProcessEventRequest(request) => Ok(self
                    .controller_service
                    .process_event(request)
                    .encode_to_vec()),
                any_request::Request::ProcessQueryRequest(request) => Ok(self
                    .controller_service
                    .process_query(request)
                    .encode_to_vec()),
                any_request::Request::ReadEventsRequest(request) => {
                    Ok(self.monitor_service.read_events(request).encode_to_vec())
                }
                any_request::Request::OpenSinkRequest(request) => {
                    Ok(self.monitor_service.open_sink(request).encode_to_vec())
                }
                any_request::Request::CloseSinkRequest(request) => {
                    Ok(self.monitor_service.close_sink(request).encode_to_vec())
                }
            },
            Ok(AnyRequest { request: None }) => Err(InvalidRequest {
                description: "the message did not contain any request".to_string(),
            }),
            Err(err) => Err(InvalidRequest {
                description: format!("bad request: {}", err),
            }),
        }
    }

    /// Initialize a simulation with the provided time.
    ///
    /// If a simulation is already active, it is destructed and replaced with a
    /// new simulation.
    ///
    /// If the initialization time is not provided, it is initialized with the
    /// epoch of `MonotonicTime` (1970-01-01 00:00:00 TAI).
    fn init(&mut self, request: InitRequest) -> InitReply {
        let start_time = request.time.unwrap_or_default();
        let reply = if let Some(start_time) = timestamp_to_monotonic(start_time) {
            let (sim_init, endpoint_registry) = (self.sim_gen)();
            let simulation = sim_init.init(start_time);
            self.controller_service = ControllerService::Started {
                simulation,
                event_source_registry: endpoint_registry.event_source_registry,
                query_source_registry: endpoint_registry.query_source_registry,
                key_registry: KeyRegistry::default(),
            };
            self.monitor_service = MonitorService::Started {
                event_sink_registry: endpoint_registry.event_sink_registry,
            };

            init_reply::Result::Empty(())
        } else {
            init_reply::Result::Error(Error {
                code: ErrorCode::InvalidTime as i32,
                message: "out-of-range nanosecond field".to_string(),
            })
        };

        InitReply {
            result: Some(reply),
        }
    }
}

impl fmt::Debug for ProtobufService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtobufService").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InvalidRequest {
    description: String,
}

impl fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

impl error::Error for InvalidRequest {}
