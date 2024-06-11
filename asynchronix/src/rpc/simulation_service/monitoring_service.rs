use std::error;
use std::fmt;

use crate::rpc::SinkRegistry;

use super::super::codegen::simulation::*;

/// Protobuf-based simulation monitor.
///
/// A `MonitoringService` enables the monitoring of the event sinks of a
/// [`Simulation`](crate::simulation::Simulation).
///
/// Its methods map the various RPC monitoring service methods defined in
/// `simulation.proto`.
pub(crate) struct MonitoringService {
    sink_registry: SinkRegistry,
}

impl MonitoringService {
    /// Creates a new `MonitoringService`.
    pub(crate) fn new(sink_registry: SinkRegistry) -> Self {
        Self { sink_registry }
    }

    /// Read all events from an event sink.
    pub(crate) fn read_events(&mut self, request: ReadEventsRequest) -> ReadEventsReply {
        let reply = move || -> Result<Vec<Vec<u8>>, (ErrorCode, String)> {
            let sink_name = &request.sink_name;

            let sink = self.sink_registry.get_event_sink_mut(sink_name).ok_or((
                ErrorCode::SinkNotFound,
                "no sink is registered with the name '{}'".to_string(),
            ))?;

            sink.collect().map_err(|_| {
                (
                    ErrorCode::InvalidMessage,
                    format!(
                        "the event could not be serialized from type '{}'",
                        sink.event_type_name()
                    ),
                )
            })
        }();

        match reply {
            Ok(events) => ReadEventsReply {
                events,
                result: Some(read_events_reply::Result::Empty(())),
            },
            Err((code, message)) => ReadEventsReply {
                events: Vec::new(),
                result: Some(read_events_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                })),
            },
        }
    }

    /// Opens an event sink.
    pub(crate) fn open_sink(&mut self, request: OpenSinkRequest) -> OpenSinkReply {
        let reply = move || -> Result<(), (ErrorCode, String)> {
            let sink_name = &request.sink_name;

            let sink = self.sink_registry.get_event_sink_mut(sink_name).ok_or((
                ErrorCode::SinkNotFound,
                "no sink is registered with the name '{}'".to_string(),
            ))?;

            sink.open();

            Ok(())
        }();

        match reply {
            Ok(()) => OpenSinkReply {
                result: Some(open_sink_reply::Result::Empty(())),
            },
            Err((code, message)) => OpenSinkReply {
                result: Some(open_sink_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                })),
            },
        }
    }

    /// Closes an event sink.
    pub(crate) fn close_sink(&mut self, request: CloseSinkRequest) -> CloseSinkReply {
        let reply = move || -> Result<(), (ErrorCode, String)> {
            let sink_name = &request.sink_name;

            let sink = self.sink_registry.get_event_sink_mut(sink_name).ok_or((
                ErrorCode::SinkNotFound,
                "no sink is registered with the name '{}'".to_string(),
            ))?;

            sink.close();

            Ok(())
        }();

        match reply {
            Ok(()) => CloseSinkReply {
                result: Some(close_sink_reply::Result::Empty(())),
            },
            Err((code, message)) => CloseSinkReply {
                result: Some(close_sink_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                })),
            },
        }
    }
}

impl fmt::Debug for MonitoringService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimulationService").finish_non_exhaustive()
    }
}
