use std::fmt;

use prost_types::Timestamp;

use crate::registry::{EventSourceRegistry, QuerySourceRegistry};
use crate::rpc::key_registry::{KeyRegistry, KeyRegistryId};
use crate::simulation::Simulation;

use super::super::codegen::simulation::*;
use super::{
    monotonic_to_timestamp, simulation_not_started_error, timestamp_to_monotonic, to_error,
    to_positive_duration, to_strictly_positive_duration,
};

/// Protobuf-based simulation manager.
///
/// A `ControllerService` enables the management of the lifecycle of a
/// simulation.
///
/// Its methods map the various RPC simulation control service methods defined
/// in `simulation.proto`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ControllerService {
    NotStarted,
    Started {
        simulation: Simulation,
        event_source_registry: EventSourceRegistry,
        query_source_registry: QuerySourceRegistry,
        key_registry: KeyRegistry,
    },
}

impl ControllerService {
    /// Returns the current simulation time.
    pub(crate) fn time(&mut self, _request: TimeRequest) -> TimeReply {
        let reply = match self {
            Self::Started { simulation, .. } => {
                if let Some(timestamp) = monotonic_to_timestamp(simulation.time()) {
                    time_reply::Result::Time(timestamp)
                } else {
                    time_reply::Result::Error(to_error(
                        ErrorCode::SimulationTimeOutOfRange,
                        "the final simulation time is out of range",
                    ))
                }
            }
            Self::NotStarted => time_reply::Result::Error(simulation_not_started_error()),
        };

        TimeReply {
            result: Some(reply),
        }
    }

    /// Advances simulation time to that of the next scheduled event, processing
    /// that event as well as all other event scheduled for the same time.
    ///
    /// Processing is gated by a (possibly blocking) call to
    /// [`Clock::synchronize()`](crate::time::Clock::synchronize) on the
    /// configured simulation clock. This method blocks until all newly
    /// processed events have completed.
    pub(crate) fn step(&mut self, _request: StepRequest) -> StepReply {
        let reply = match self {
            Self::Started { simulation, .. } => {
                simulation.step();

                if let Some(timestamp) = monotonic_to_timestamp(simulation.time()) {
                    step_reply::Result::Time(timestamp)
                } else {
                    step_reply::Result::Error(to_error(
                        ErrorCode::SimulationTimeOutOfRange,
                        "the final simulation time is out of range",
                    ))
                }
            }
            Self::NotStarted => step_reply::Result::Error(simulation_not_started_error()),
        };

        StepReply {
            result: Some(reply),
        }
    }

    /// Iteratively advances the simulation time until the specified deadline,
    /// as if by calling
    /// [`Simulation::step()`](crate::simulation::Simulation::step) repeatedly.
    ///
    /// This method blocks until all events scheduled up to the specified target
    /// time have completed. The simulation time upon completion is equal to the
    /// specified target time, whether or not an event was scheduled for that
    /// time.
    pub(crate) fn step_until(&mut self, request: StepUntilRequest) -> StepUntilReply {
        let reply = match self {
            Self::Started { simulation, .. } => move || -> Result<Timestamp, Error> {
                let deadline = request.deadline.ok_or(to_error(
                    ErrorCode::MissingArgument,
                    "missing deadline argument",
                ))?;

                match deadline {
                    step_until_request::Deadline::Time(time) => {
                        let time = timestamp_to_monotonic(time).ok_or(to_error(
                            ErrorCode::InvalidTime,
                            "out-of-range nanosecond field",
                        ))?;

                        simulation.step_until(time).map_err(|_| {
                            to_error(
                                ErrorCode::InvalidTime,
                                "the specified deadline lies in the past",
                            )
                        })?;
                    }
                    step_until_request::Deadline::Duration(duration) => {
                        let duration = to_positive_duration(duration).ok_or(to_error(
                            ErrorCode::InvalidDuration,
                            "the specified deadline lies in the past",
                        ))?;

                        simulation.step_by(duration);
                    }
                };

                let timestamp = monotonic_to_timestamp(simulation.time()).ok_or(to_error(
                    ErrorCode::SimulationTimeOutOfRange,
                    "the final simulation time is out of range",
                ))?;

                Ok(timestamp)
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        StepUntilReply {
            result: Some(match reply {
                Ok(timestamp) => step_until_reply::Result::Time(timestamp),
                Err(error) => step_until_reply::Result::Error(error),
            }),
        }
    }

    /// Schedules an event at a future time.
    pub(crate) fn schedule_event(&mut self, request: ScheduleEventRequest) -> ScheduleEventReply {
        let reply = match self {
            Self::Started {
                simulation,
                event_source_registry,
                key_registry,
                ..
            } => move || -> Result<Option<KeyRegistryId>, Error> {
                let source_name = &request.source_name;
                let event = &request.event;
                let with_key = request.with_key;
                let period = request
                    .period
                    .map(|period| {
                        to_strictly_positive_duration(period).ok_or(to_error(
                            ErrorCode::InvalidDuration,
                            "the specified event period is not strictly positive",
                        ))
                    })
                    .transpose()?;

                let deadline = request.deadline.ok_or(to_error(
                    ErrorCode::MissingArgument,
                    "missing deadline argument",
                ))?;

                let deadline = match deadline {
                    schedule_event_request::Deadline::Time(time) => timestamp_to_monotonic(time)
                        .ok_or(to_error(
                            ErrorCode::InvalidTime,
                            "out-of-range nanosecond field",
                        ))?,
                    schedule_event_request::Deadline::Duration(duration) => {
                        let duration = to_strictly_positive_duration(duration).ok_or(to_error(
                            ErrorCode::InvalidDuration,
                            "the specified scheduling deadline is not in the future",
                        ))?;

                        simulation.time() + duration
                    }
                };

                let source = event_source_registry.get_mut(source_name).ok_or(to_error(
                    ErrorCode::SourceNotFound,
                    "no event source is registered with the name '{}'".to_string(),
                ))?;

                let (action, action_key) = match (with_key, period) {
                    (false, None) => source.event(event).map(|action| (action, None)),
                    (false, Some(period)) => source
                        .periodic_event(period, event)
                        .map(|action| (action, None)),
                    (true, None) => source
                        .keyed_event(event)
                        .map(|(action, key)| (action, Some(key))),
                    (true, Some(period)) => source
                        .keyed_periodic_event(period, event)
                        .map(|(action, key)| (action, Some(key))),
                }
                .map_err(|e| {
                    to_error(
                        ErrorCode::InvalidMessage,
                        format!(
                            "the event could not be deserialized as type '{}': {}",
                            source.event_type_name(),
                            e
                        ),
                    )
                })?;

                let key_id = action_key.map(|action_key| {
                    // Free stale keys from the registry.
                    key_registry.remove_expired_keys(simulation.time());

                    if period.is_some() {
                        key_registry.insert_eternal_key(action_key)
                    } else {
                        key_registry.insert_key(action_key, deadline)
                    }
                });

                simulation.process(action);

                Ok(key_id)
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        ScheduleEventReply {
            result: Some(match reply {
                Ok(Some(key_id)) => {
                    let (subkey1, subkey2) = key_id.into_raw_parts();
                    schedule_event_reply::Result::Key(EventKey {
                        subkey1: subkey1
                            .try_into()
                            .expect("action key index is too large to be serialized"),
                        subkey2,
                    })
                }
                Ok(None) => schedule_event_reply::Result::Empty(()),
                Err(error) => schedule_event_reply::Result::Error(error),
            }),
        }
    }

    /// Cancels a keyed event.
    pub(crate) fn cancel_event(&mut self, request: CancelEventRequest) -> CancelEventReply {
        let reply = match self {
            Self::Started {
                simulation,
                key_registry,
                ..
            } => move || -> Result<(), Error> {
                let key = request
                    .key
                    .ok_or(to_error(ErrorCode::MissingArgument, "missing key argument"))?;
                let subkey1: usize = key
                    .subkey1
                    .try_into()
                    .map_err(|_| to_error(ErrorCode::InvalidKey, "invalid event key"))?;
                let subkey2 = key.subkey2;

                let key_id = KeyRegistryId::from_raw_parts(subkey1, subkey2);

                key_registry.remove_expired_keys(simulation.time());
                let key = key_registry.extract_key(key_id).ok_or(to_error(
                    ErrorCode::InvalidKey,
                    "invalid or expired event key",
                ))?;

                key.cancel();

                Ok(())
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        CancelEventReply {
            result: Some(match reply {
                Ok(()) => cancel_event_reply::Result::Empty(()),
                Err(error) => cancel_event_reply::Result::Error(error),
            }),
        }
    }

    /// Broadcasts an event from an event source immediately, blocking until
    /// completion.
    ///
    /// Simulation time remains unchanged.
    pub(crate) fn process_event(&mut self, request: ProcessEventRequest) -> ProcessEventReply {
        let reply = match self {
            Self::Started {
                simulation,
                event_source_registry,
                ..
            } => move || -> Result<(), Error> {
                let source_name = &request.source_name;
                let event = &request.event;

                let source = event_source_registry.get_mut(source_name).ok_or(to_error(
                    ErrorCode::SourceNotFound,
                    "no source is registered with the name '{}'".to_string(),
                ))?;

                let event = source.event(event).map_err(|e| {
                    to_error(
                        ErrorCode::InvalidMessage,
                        format!(
                            "the event could not be deserialized as type '{}': {}",
                            source.event_type_name(),
                            e
                        ),
                    )
                })?;

                simulation.process(event);

                Ok(())
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        ProcessEventReply {
            result: Some(match reply {
                Ok(()) => process_event_reply::Result::Empty(()),
                Err(error) => process_event_reply::Result::Error(error),
            }),
        }
    }

    /// Broadcasts an event from an event source immediately, blocking until
    /// completion.
    ///
    /// Simulation time remains unchanged.
    pub(crate) fn process_query(&mut self, request: ProcessQueryRequest) -> ProcessQueryReply {
        let reply = match self {
            Self::Started {
                simulation,
                query_source_registry,
                ..
            } => move || -> Result<Vec<Vec<u8>>, Error> {
                let source_name = &request.source_name;
                let request = &request.request;

                let source = query_source_registry.get_mut(source_name).ok_or(to_error(
                    ErrorCode::SourceNotFound,
                    "no source is registered with the name '{}'".to_string(),
                ))?;

                let (query, mut promise) = source.query(request).map_err(|e| {
                    to_error(
                        ErrorCode::InvalidMessage,
                        format!(
                            "the request could not be deserialized as type '{}': {}",
                            source.request_type_name(),
                            e
                        ),
                    )
                })?;

                simulation.process(query);

                let replies = promise.take_collect().ok_or(to_error(
                    ErrorCode::InternalError,
                    "a reply to the query was expected but none was available".to_string(),
                ))?;

                replies.map_err(|e| {
                    to_error(
                        ErrorCode::InvalidMessage,
                        format!(
                            "the reply could not be serialized as type '{}': {}",
                            source.reply_type_name(),
                            e
                        ),
                    )
                })
            }(),
            Self::NotStarted => Err(simulation_not_started_error()),
        };

        match reply {
            Ok(replies) => ProcessQueryReply {
                replies,
                result: Some(process_query_reply::Result::Empty(())),
            },
            Err(error) => ProcessQueryReply {
                replies: Vec::new(),
                result: Some(process_query_reply::Result::Error(error)),
            },
        }
    }
}

impl fmt::Debug for ControllerService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControllerService").finish_non_exhaustive()
    }
}
