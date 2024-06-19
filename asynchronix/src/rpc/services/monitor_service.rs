use std::fmt;

use crate::registry::EventSinkRegistry;

use super::super::codegen::simulation::*;
use super::{simulation_not_started_error, to_error};

/// Protobuf-based simulation monitor.
///
/// A `MonitorService` enables the monitoring of the event sinks of a
/// [`Simulation`](crate::simulation::Simulation).
///
/// Its methods map the various RPC monitoring service methods defined in
/// `simulation.proto`.
pub(crate) enum MonitorService {
    Started {
        event_sink_registry: EventSinkRegistry,
    },
    NotStarted,
}

impl MonitorService {
    /// Read all events from an event sink.
    pub(crate) fn read_events(&mut self, request: ReadEventsRequest) -> ReadEventsReply {
        let reply = match self {
            Self::Started {
                event_sink_registry,
            } => move || -> Result<Vec<Vec<u8>>, Error> {
                let sink_name = &request.sink_name;

                let sink = event_sink_registry.get_mut(sink_name).ok_or(to_error(
                    ErrorCode::SinkNotFound,
                    format!("no sink is registered with the name '{}'", sink_name),
                ))?;

                sink.collect().map_err(|e| {
                    to_error(
                        ErrorCode::InvalidMessage,
                        format!(
                            "the event could not be serialized from type '{}': {}",
                            sink.event_type_name(),
                            e
                        ),
                    )
                })
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        match reply {
            Ok(events) => ReadEventsReply {
                events,
                result: Some(read_events_reply::Result::Empty(())),
            },
            Err(error) => ReadEventsReply {
                events: Vec::new(),
                result: Some(read_events_reply::Result::Error(error)),
            },
        }
    }

    /// Opens an event sink.
    pub(crate) fn open_sink(&mut self, request: OpenSinkRequest) -> OpenSinkReply {
        let reply = match self {
            Self::Started {
                event_sink_registry,
            } => {
                let sink_name = &request.sink_name;

                if let Some(sink) = event_sink_registry.get_mut(sink_name) {
                    sink.open();

                    open_sink_reply::Result::Empty(())
                } else {
                    open_sink_reply::Result::Error(to_error(
                        ErrorCode::SinkNotFound,
                        format!("no sink is registered with the name '{}'", sink_name),
                    ))
                }
            }
            Self::NotStarted => open_sink_reply::Result::Error(simulation_not_started_error()),
        };

        OpenSinkReply {
            result: Some(reply),
        }
    }

    /// Closes an event sink.
    pub(crate) fn close_sink(&mut self, request: CloseSinkRequest) -> CloseSinkReply {
        let reply = match self {
            Self::Started {
                event_sink_registry,
            } => {
                let sink_name = &request.sink_name;

                if let Some(sink) = event_sink_registry.get_mut(sink_name) {
                    sink.close();

                    close_sink_reply::Result::Empty(())
                } else {
                    close_sink_reply::Result::Error(to_error(
                        ErrorCode::SinkNotFound,
                        format!("no sink is registered with the name '{}'", sink_name),
                    ))
                }
            }
            Self::NotStarted => close_sink_reply::Result::Error(simulation_not_started_error()),
        };

        CloseSinkReply {
            result: Some(reply),
        }
    }
}

impl fmt::Debug for MonitorService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimulationService").finish_non_exhaustive()
    }
}
