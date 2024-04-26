//! GRPC simulation server.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tonic::{transport::Server, Request, Response, Status};

use crate::rpc::EndpointRegistry;
use crate::simulation::SimInit;

use super::codegen::simulation::*;
use super::generic_server::GenericServer;

/// Runs a GRPC simulation server.
///
/// The first argument is a closure that is called every time the simulation is
/// started by the remote client. It must create a new `SimInit` object
/// complemented by a registry that exposes the public event and query
/// interface.
pub fn run<F>(sim_gen: F, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
{
    // Use a single-threaded server.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;

    let sim_manager = GrpcServer::new(sim_gen);

    rt.block_on(async move {
        Server::builder()
            .add_service(simulation_server::SimulationServer::new(sim_manager))
            .serve(addr)
            .await?;

        Ok(())
    })
}

struct GrpcServer<F>
where
    F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
{
    inner: Mutex<GenericServer<F>>,
}

impl<F> GrpcServer<F>
where
    F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
{
    fn new(sim_gen: F) -> Self {
        Self {
            inner: Mutex::new(GenericServer::new(sim_gen)),
        }
    }

    fn inner(&self) -> MutexGuard<'_, GenericServer<F>> {
        self.inner.lock().unwrap()
    }
}

#[tonic::async_trait]
impl<F> simulation_server::Simulation for GrpcServer<F>
where
    F: FnMut() -> (SimInit, EndpointRegistry) + Send + 'static,
{
    async fn init(&self, request: Request<InitRequest>) -> Result<Response<InitReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().init(request)))
    }
    async fn time(&self, request: Request<TimeRequest>) -> Result<Response<TimeReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().time(request)))
    }
    async fn step(&self, request: Request<StepRequest>) -> Result<Response<StepReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().step(request)))
    }
    async fn step_until(
        &self,
        request: Request<StepUntilRequest>,
    ) -> Result<Response<StepUntilReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().step_until(request)))
    }
    async fn schedule_event(
        &self,
        request: Request<ScheduleEventRequest>,
    ) -> Result<Response<ScheduleEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().schedule_event(request)))
    }
    async fn cancel_event(
        &self,
        request: Request<CancelEventRequest>,
    ) -> Result<Response<CancelEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().cancel_event(request)))
    }
    async fn process_event(
        &self,
        request: Request<ProcessEventRequest>,
    ) -> Result<Response<ProcessEventReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().process_event(request)))
    }
    async fn process_query(
        &self,
        request: Request<ProcessQueryRequest>,
    ) -> Result<Response<ProcessQueryReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().process_query(request)))
    }
    async fn read_events(
        &self,
        request: Request<ReadEventsRequest>,
    ) -> Result<Response<ReadEventsReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().read_events(request)))
    }
    async fn open_sink(
        &self,
        request: Request<OpenSinkRequest>,
    ) -> Result<Response<OpenSinkReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().open_sink(request)))
    }
    async fn close_sink(
        &self,
        request: Request<CloseSinkRequest>,
    ) -> Result<Response<CloseSinkReply>, Status> {
        let request = request.into_inner();

        Ok(Response::new(self.inner().close_sink(request)))
    }
}
