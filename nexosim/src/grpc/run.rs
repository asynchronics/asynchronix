//! gRPC simulation service.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::MutexGuard;

use serde::de::DeserializeOwned;
use tonic::{transport::Server, Request, Response, Status};

use crate::registry::EndpointRegistry;
use crate::simulation::{Scheduler, Simulation, SimulationError};

use super::codegen::simulation::*;
use super::key_registry::KeyRegistry;
use super::services::InitService;
use super::services::{ControllerService, MonitorService};

/// Runs a gRPC simulation server.
///
/// The first argument is a closure that takes an initialization configuration
/// and is called every time the simulation is (re)started by the remote client.
/// It must create a new simulation and its scheduler, complemented by a
/// registry that exposes the public event and query interface.
pub fn run<F, I>(sim_gen: F, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(I) -> Result<(Simulation, Scheduler, EndpointRegistry), SimulationError>
        + Send
        + 'static,
    I: DeserializeOwned,
{
    run_service(GrpcSimulationService::new(sim_gen), addr)
}

/// Monomorphization of the networking code.
///
/// Keeping this as a separate monomorphized fragment can even triple
/// compilation speed for incremental release builds.
fn run_service(
    service: GrpcSimulationService,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    // Use 2 threads so that the controller and monitor services can be used
    // concurrently.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .build()?;

    rt.block_on(async move {
        Server::builder()
            .add_service(simulation_server::SimulationServer::new(service))
            .serve(addr)
            .await?;

        Ok(())
    })
}

struct GrpcSimulationService {
    init_service: Mutex<InitService>,
    controller_service: Mutex<ControllerService>,
    monitor_service: Mutex<MonitorService>,
}

impl GrpcSimulationService {
    /// Creates a new `GrpcSimulationService` without any active simulation.
    ///
    /// The argument is a closure that takes an initialization configuration and
    /// is called every time the simulation is (re)started by the remote client.
    /// It must create a new simulation and its scheduler, complemented by a
    /// registry that exposes the public event and query interface.
    pub(crate) fn new<F, I>(sim_gen: F) -> Self
    where
        F: FnMut(I) -> Result<(Simulation, Scheduler, EndpointRegistry), SimulationError>
            + Send
            + 'static,
        I: DeserializeOwned,
    {
        Self {
            init_service: Mutex::new(InitService::new(sim_gen)),
            controller_service: Mutex::new(ControllerService::NotStarted),
            monitor_service: Mutex::new(MonitorService::NotStarted),
        }
    }

    /// Locks the initializer and returns the mutex guard.
    fn initializer(&self) -> MutexGuard<'_, InitService> {
        self.init_service.lock().unwrap()
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

        let (reply, bench) = self.initializer().init(request);

        if let Some((simulation, scheduler, endpoint_registry)) = bench {
            *self.controller() = ControllerService::Started {
                simulation,
                scheduler,
                event_source_registry: endpoint_registry.event_source_registry,
                query_source_registry: endpoint_registry.query_source_registry,
                key_registry: KeyRegistry::default(),
            };
            *self.monitor() = MonitorService::Started {
                event_sink_registry: endpoint_registry.event_sink_registry,
            };
        }

        Ok(Response::new(reply))
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
