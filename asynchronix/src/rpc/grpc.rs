//! gRPC simulation service.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tonic::{transport::Server, Request, Response, Status};

use crate::registry::EndpointRegistry;
use crate::simulation::SimInit;

use super::codegen::simulation::*;
use super::key_registry::KeyRegistry;
use super::services::{timestamp_to_monotonic, ControllerService, MonitorService};

/// Runs a gRPC simulation server.
///
/// The first argument is a closure that is called every time the simulation is
/// (re)started by the remote client. It must create a new `SimInit` object
/// complemented by a registry that exposes the public event and query
/// interface.
pub fn run<F>(sim_gen: F, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
{
    // Use 2 threads so that the controller and monitor services can be used
    // concurrently.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .build()?;

    let sim_manager = GrpcSimulationService::new(sim_gen);

    rt.block_on(async move {
        Server::builder()
            .add_service(simulation_server::SimulationServer::new(sim_manager))
            .serve(addr)
            .await?;

        Ok(())
    })
}

struct GrpcSimulationService {
    sim_gen: Mutex<Box<dyn FnMut() -> (SimInit, EndpointRegistry) + Send + 'static>>,
    controller_service: Mutex<ControllerService>,
    monitor_service: Mutex<MonitorService>,
}

impl GrpcSimulationService {
    /// Creates a new `GrpcSimulationService` without any active simulation.
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
            sim_gen: Mutex::new(Box::new(sim_gen)),
            controller_service: Mutex::new(ControllerService::NotStarted),
            monitor_service: Mutex::new(MonitorService::NotStarted),
        }
    }

    /// Locks the controller and returns the mutex guard.
    fn controller(&self) -> MutexGuard<'_, ControllerService> {
        self.controller_service.lock().unwrap()
    }

    /// Locks the monitor and returns the mutex guard.
    fn monitor(&self) -> MutexGuard<'_, MonitorService> {
        self.monitor_service.lock().unwrap()
    }
}

#[tonic::async_trait]
impl simulation_server::Simulation for GrpcSimulationService {
    async fn init(&self, request: Request<InitRequest>) -> Result<Response<InitReply>, Status> {
        let request = request.into_inner();

        let start_time = request.time.unwrap_or_default();
        let reply = if let Some(start_time) = timestamp_to_monotonic(start_time) {
            let (sim_init, endpoint_registry) = (self.sim_gen.lock().unwrap())();
            let simulation = sim_init.init(start_time);
            *self.controller() = ControllerService::Started {
                simulation,
                event_source_registry: endpoint_registry.event_source_registry,
                query_source_registry: endpoint_registry.query_source_registry,
                key_registry: KeyRegistry::default(),
            };
            *self.monitor() = MonitorService::Started {
                event_sink_registry: endpoint_registry.event_sink_registry,
            };

            init_reply::Result::Empty(())
        } else {
            init_reply::Result::Error(Error {
                code: ErrorCode::InvalidTime as i32,
                message: "out-of-range nanosecond field".to_string(),
            })
        };

        Ok(Response::new(InitReply {
            result: Some(reply),
        }))
    }
    async fn time(&self, request: Request<TimeRequest>) -> Result<Response<TimeReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().time(request)))
    }
    async fn step(&self, request: Request<StepRequest>) -> Result<Response<StepReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().step(request)))
    }
    async fn step_until(
        &self,
        request: Request<StepUntilRequest>,
    ) -> Result<Response<StepUntilReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().step_until(request)))
    }
    async fn schedule_event(
        &self,
        request: Request<ScheduleEventRequest>,
    ) -> Result<Response<ScheduleEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().schedule_event(request)))
    }
    async fn cancel_event(
        &self,
        request: Request<CancelEventRequest>,
    ) -> Result<Response<CancelEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().cancel_event(request)))
    }
    async fn process_event(
        &self,
        request: Request<ProcessEventRequest>,
    ) -> Result<Response<ProcessEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().process_event(request)))
    }
    async fn process_query(
        &self,
        request: Request<ProcessQueryRequest>,
    ) -> Result<Response<ProcessQueryReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.controller().process_query(request)))
    }
    async fn read_events(
        &self,
        request: Request<ReadEventsRequest>,
    ) -> Result<Response<ReadEventsReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.monitor().read_events(request)))
    }
    async fn open_sink(
        &self,
        request: Request<OpenSinkRequest>,
    ) -> Result<Response<OpenSinkReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.monitor().open_sink(request)))
    }
    async fn close_sink(
        &self,
        request: Request<CloseSinkRequest>,
    ) -> Result<Response<CloseSinkReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.monitor().close_sink(request)))
    }
}
