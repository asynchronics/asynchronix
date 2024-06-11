use std::fmt;
use std::time::Duration;

use prost_types::Timestamp;
use tai_time::MonotonicTime;

use crate::rpc::key_registry::{KeyRegistry, KeyRegistryId};
use crate::rpc::SourceRegistry;
use crate::simulation::Simulation;

use super::super::codegen::simulation::*;

/// Protobuf-based simulation manager.
///
/// A `ManagementService` enables the management of the lifecycle of a
/// simulation.
///
/// Its methods map the various RPC management service methods defined in
/// `simulation.proto`.
pub(crate) struct ManagementService {
    simulation: Simulation,
    source_registry: SourceRegistry,
    key_registry: KeyRegistry,
}

impl ManagementService {
    /// Creates a new `ManagementService`.
    pub(crate) fn new(
        simulation: Simulation,
        source_registry: SourceRegistry,
        key_registry: KeyRegistry,
    ) -> Self {
        Self {
            simulation,
            source_registry,
            key_registry,
        }
    }

    /// Returns the current simulation time.
    pub(crate) fn time(&mut self, _request: TimeRequest) -> TimeReply {
        let reply = if let Some(timestamp) = monotonic_to_timestamp(self.simulation.time()) {
            time_reply::Result::Time(timestamp)
        } else {
            time_reply::Result::Error(Error {
                code: ErrorCode::SimulationTimeOutOfRange as i32,
                message: "the final simulation time is out of range".to_string(),
            })
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
        self.simulation.step();

        let reply = if let Some(timestamp) = monotonic_to_timestamp(self.simulation.time()) {
            step_reply::Result::Time(timestamp)
        } else {
            step_reply::Result::Error(Error {
                code: ErrorCode::SimulationTimeOutOfRange as i32,
                message: "the final simulation time is out of range".to_string(),
            })
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
        let reply = move || -> Result<Timestamp, (ErrorCode, &str)> {
            let deadline = request
                .deadline
                .ok_or((ErrorCode::MissingArgument, "missing deadline argument"))?;

            match deadline {
                step_until_request::Deadline::Time(time) => {
                    let time = timestamp_to_monotonic(time)
                        .ok_or((ErrorCode::InvalidTime, "out-of-range nanosecond field"))?;

                    self.simulation.step_until(time).map_err(|_| {
                        (
                            ErrorCode::InvalidTime,
                            "the specified deadline lies in the past",
                        )
                    })?;
                }
                step_until_request::Deadline::Duration(duration) => {
                    let duration = to_positive_duration(duration).ok_or((
                        ErrorCode::InvalidDuration,
                        "the specified deadline lies in the past",
                    ))?;

                    self.simulation.step_by(duration);
                }
            };

            let timestamp = monotonic_to_timestamp(self.simulation.time()).ok_or((
                ErrorCode::SimulationTimeOutOfRange,
                "the final simulation time is out of range",
            ))?;

            Ok(timestamp)
        }();

        StepUntilReply {
            result: Some(match reply {
                Ok(timestamp) => step_until_reply::Result::Time(timestamp),
                Err((code, message)) => step_until_reply::Result::Error(Error {
                    code: code as i32,
                    message: message.to_string(),
                }),
            }),
        }
    }

    /// Schedules an event at a future time.
    pub(crate) fn schedule_event(&mut self, request: ScheduleEventRequest) -> ScheduleEventReply {
        let reply = move || -> Result<Option<KeyRegistryId>, (ErrorCode, String)> {
            let source_name = &request.source_name;
            let msgpack_event = &request.event;
            let with_key = request.with_key;
            let period = request
                .period
                .map(|period| {
                    to_strictly_positive_duration(period).ok_or((
                        ErrorCode::InvalidDuration,
                        "the specified event period is not strictly positive".to_string(),
                    ))
                })
                .transpose()?;

            let deadline = request.deadline.ok_or((
                ErrorCode::MissingArgument,
                "missing deadline argument".to_string(),
            ))?;

            let deadline = match deadline {
                schedule_event_request::Deadline::Time(time) => timestamp_to_monotonic(time)
                    .ok_or((
                        ErrorCode::InvalidTime,
                        "out-of-range nanosecond field".to_string(),
                    ))?,
                schedule_event_request::Deadline::Duration(duration) => {
                    let duration = to_strictly_positive_duration(duration).ok_or((
                        ErrorCode::InvalidDuration,
                        "the specified scheduling deadline is not in the future".to_string(),
                    ))?;

                    self.simulation.time() + duration
                }
            };

            let source = self
                .source_registry
                .get_event_source_mut(source_name)
                .ok_or((
                    ErrorCode::SourceNotFound,
                    "no event source is registered with the name '{}'".to_string(),
                ))?;

            let (action, action_key) = match (with_key, period) {
                (false, None) => source.event(msgpack_event).map(|action| (action, None)),
                (false, Some(period)) => source
                    .periodic_event(period, msgpack_event)
                    .map(|action| (action, None)),
                (true, None) => source
                    .keyed_event(msgpack_event)
                    .map(|(action, key)| (action, Some(key))),
                (true, Some(period)) => source
                    .keyed_periodic_event(period, msgpack_event)
                    .map(|(action, key)| (action, Some(key))),
            }
            .map_err(|_| {
                (
                    ErrorCode::InvalidMessage,
                    format!(
                        "the event could not be deserialized as type '{}'",
                        source.event_type_name()
                    ),
                )
            })?;

            let key_id = action_key.map(|action_key| {
                // Free stale keys from the registry.
                self.key_registry
                    .remove_expired_keys(self.simulation.time());

                if period.is_some() {
                    self.key_registry.insert_eternal_key(action_key)
                } else {
                    self.key_registry.insert_key(action_key, deadline)
                }
            });

            self.simulation.process(action);

            Ok(key_id)
        }();

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
                Err((code, message)) => schedule_event_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                }),
            }),
        }
    }

    /// Cancels a keyed event.
    pub(crate) fn cancel_event(&mut self, request: CancelEventRequest) -> CancelEventReply {
        let reply = move || -> Result<(), (ErrorCode, String)> {
            let key = request.key.ok_or((
                ErrorCode::MissingArgument,
                "missing key argument".to_string(),
            ))?;
            let subkey1: usize = key
                .subkey1
                .try_into()
                .map_err(|_| (ErrorCode::InvalidKey, "invalid event key".to_string()))?;
            let subkey2 = key.subkey2;

            let key_id = KeyRegistryId::from_raw_parts(subkey1, subkey2);

            self.key_registry
                .remove_expired_keys(self.simulation.time());
            let key = self.key_registry.extract_key(key_id).ok_or((
                ErrorCode::InvalidKey,
                "invalid or expired event key".to_string(),
            ))?;

            key.cancel();

            Ok(())
        }();

        CancelEventReply {
            result: Some(match reply {
                Ok(()) => cancel_event_reply::Result::Empty(()),
                Err((code, message)) => cancel_event_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                }),
            }),
        }
    }

    /// Broadcasts an event from an event source immediately, blocking until
    /// completion.
    ///
    /// Simulation time remains unchanged.
    pub(crate) fn process_event(&mut self, request: ProcessEventRequest) -> ProcessEventReply {
        let reply = move || -> Result<(), (ErrorCode, String)> {
            let source_name = &request.source_name;
            let msgpack_event = &request.event;

            let source = self
                .source_registry
                .get_event_source_mut(source_name)
                .ok_or((
                    ErrorCode::SourceNotFound,
                    "no source is registered with the name '{}'".to_string(),
                ))?;

            let event = source.event(msgpack_event).map_err(|_| {
                (
                    ErrorCode::InvalidMessage,
                    format!(
                        "the event could not be deserialized as type '{}'",
                        source.event_type_name()
                    ),
                )
            })?;

            self.simulation.process(event);

            Ok(())
        }();

        ProcessEventReply {
            result: Some(match reply {
                Ok(()) => process_event_reply::Result::Empty(()),
                Err((code, message)) => process_event_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                }),
            }),
        }
    }

    /// Broadcasts an event from an event source immediately, blocking until
    /// completion.
    ///
    /// Simulation time remains unchanged.
    pub(crate) fn process_query(&mut self, request: ProcessQueryRequest) -> ProcessQueryReply {
        let reply = move || -> Result<Vec<Vec<u8>>, (ErrorCode, String)> {
            let source_name = &request.source_name;
            let msgpack_request = &request.request;

            let source = self
                .source_registry
                .get_query_source_mut(source_name)
                .ok_or((
                    ErrorCode::SourceNotFound,
                    "no source is registered with the name '{}'".to_string(),
                ))?;

            let (query, mut promise) = source.query(msgpack_request).map_err(|_| {
                (
                    ErrorCode::InvalidMessage,
                    format!(
                        "the request could not be deserialized as type '{}'",
                        source.request_type_name()
                    ),
                )
            })?;

            self.simulation.process(query);

            let replies = promise.take_collect().ok_or((
                ErrorCode::InternalError,
                "a reply to the query was expected but none was available".to_string(),
            ))?;

            replies.map_err(|_| {
                (
                    ErrorCode::InvalidMessage,
                    format!(
                        "the reply could not be serialized as type '{}'",
                        source.reply_type_name()
                    ),
                )
            })
        }();

        match reply {
            Ok(replies) => ProcessQueryReply {
                replies,
                result: Some(process_query_reply::Result::Empty(())),
            },
            Err((code, message)) => ProcessQueryReply {
                replies: Vec::new(),
                result: Some(process_query_reply::Result::Error(Error {
                    code: code as i32,
                    message,
                })),
            },
        }
    }
}

impl fmt::Debug for ManagementService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagementService").finish_non_exhaustive()
    }
}

/// Attempts a cast from a `MonotonicTime` to a protobuf `Timestamp`.
///
/// This will fail if the time is outside the protobuf-specified range for
/// timestamps (0001-01-01 00:00:00 to 9999-12-31 23:59:59).
fn monotonic_to_timestamp(monotonic_time: MonotonicTime) -> Option<Timestamp> {
    // Unix timestamp for 0001-01-01 00:00:00, the minimum accepted by
    // protobuf's specification for the `Timestamp` type.
    const MIN_SECS: i64 = -62135596800;
    // Unix timestamp for 9999-12-31 23:59:59, the maximum accepted by
    // protobuf's specification for the `Timestamp` type.
    const MAX_SECS: i64 = 253402300799;

    let secs = monotonic_time.as_secs();
    if !(MIN_SECS..=MAX_SECS).contains(&secs) {
        return None;
    }

    Some(Timestamp {
        seconds: secs,
        nanos: monotonic_time.subsec_nanos() as i32,
    })
}

/// Attempts a cast from a protobuf `Timestamp` to a `MonotonicTime`.
///
/// This should never fail provided that the `Timestamp` complies with the
/// protobuf specification. It can only fail if the nanosecond part is negative
/// or greater than 999'999'999.
fn timestamp_to_monotonic(timestamp: Timestamp) -> Option<MonotonicTime> {
    let nanos: u32 = timestamp.nanos.try_into().ok()?;

    MonotonicTime::new(timestamp.seconds, nanos)
}

/// Attempts a cast from a protobuf `Duration` to a `std::time::Duration`.
///
/// If the `Duration` complies with the protobuf specification, this can only
/// fail if the duration is negative.
fn to_positive_duration(duration: prost_types::Duration) -> Option<Duration> {
    if duration.seconds < 0 || duration.nanos < 0 {
        return None;
    }

    Some(Duration::new(
        duration.seconds as u64,
        duration.nanos as u32,
    ))
}

/// Attempts a cast from a protobuf `Duration` to a strictly positive
/// `std::time::Duration`.
///
/// If the `Duration` complies with the protobuf specification, this can only
/// fail if the duration is negative or null.
fn to_strictly_positive_duration(duration: prost_types::Duration) -> Option<Duration> {
    if duration.seconds < 0 || duration.nanos < 0 || (duration.seconds == 0 && duration.nanos == 0)
    {
        return None;
    }

    Some(Duration::new(
        duration.seconds as u64,
        duration.nanos as u32,
    ))
}
