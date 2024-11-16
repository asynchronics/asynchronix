//! Support for structured logging.
//!
//! # Overview
//!
//! When the `tracing` feature is activated, each tracing event or span emitted
//! by a model is wrapped in a [`tracing::Span`] with the following metadata:
//!
//! - name: `model`,
//! - target: `nexosim`,
//! - verbosity level: [`Level::INFO`](tracing::Level::INFO),
//! - a unique field called `name`, associated to the model name provided in
//!   [`SimInit::add_model`](crate::simulation::SimInit::add_model).
//!
//! The emission of `model` spans can be readily used for [event
//! filtering](#event-filtering-examples), using for instance the
//! [`tracing_subscriber::fmt`][mod@tracing_subscriber::fmt] subscriber. By
//! default, however, this subscriber will timestamp events with the wall clock
//! time. Because it is often desirable to log events using the simulation time
//! instead of (or on top of) the wall clock time, this module provides a custom
//! [`SimulationTime`] timer compatible with
//! [`tracing_subscriber::fmt`][mod@tracing_subscriber::fmt].
//!
//!
//! # Configuration
//!
//! Using the `tracing-subscriber` crate, simulation events can be logged to
//! standard output by placing the following call anywhere before
//! [`SimInit::init`](crate::simulation::SimInit::init):
//!
//! ```
//! tracing_subscriber::fmt::init();
//! ```
//!
//! Logging from a model is then a simple matter of using the `tracing` macros,
//! for instance:
//!
//! ```
//! use tracing::warn;
//!
//! pub struct MyModel { /* ... */ }
//!
//! impl MyModel {
//!     pub fn some_input_port(&mut self, _some_parameter: i32) {
//!         // ...
//!         warn!("something happened inside the simulation");
//!         // ...
//!     }
//! }
//! ```
//!
//! However, this will stamp events with the system time rather than the
//! simulation time. To use simulation time instead, a dedicated timer can be
//! configured:
//!
//! ```
//! use nexosim::tracing::SimulationTime;
//!
//! tracing_subscriber::fmt()
//!     .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
//!     .with_timer(SimulationTime::with_system_timer())
//!     .init();
//! ```
//!
//! This timer will automatically revert to system time stamping for tracing
//! events generated outside of simulation models, e.g.:
//!
//! ```text
//! [2001-02-03 04:05:06.789012345]  WARN model{name="my_model"}: my_simulation: something happened inside the simulation
//! 2024-09-10T14:39:24.670921Z  INFO my_simulation: something happened outside the simulation
//! ```
//!
//! Alternatively, `SimulationTime::with_system_timer_always()` can be used to
//! always prepend the system time even for simulation events:
//!
//! ```text
//! 2024-09-10T14:39:22.124945Z [2001-02-03 04:05:06.789012345]  WARN model{name="my_model"}: my_simulation: something happened inside the simulation
//! 2024-09-10T14:39:24.670921Z  INFO my_simulation: something happened outside the simulation
//! ```
//!
//!
//! # Event filtering examples
//!
//! Note that event filtering based on the `RUST_LOG` environment variable
//! requires the `env-filter` feature of the
//! [`tracing-subscriber`][tracing_subscriber] crate.
//!
//! The following `RUST_LOG` directive could be used to only let warnings and
//! errors pass through but still see model span information (which is emitted
//! as info):
//!
//! ```text
//! $ RUST_LOG="warn,[model]=info" cargo run --release my_simulation
//! [2001-01-01 00:00:06.000000000]  WARN model{name="kettle"}: my_simulation: water is boiling
//! [2001-01-01 00:01:36.000000000]  WARN model{name="timer"}: my_simulation: ring ring
//! [2001-01-01 00:01:36.000000000]  WARN model{name="kettle"}: my_simulation: water is ready
//! ```
//!
//! In order to see warnings or errors for the `kettle` model only, this
//! directive could be modified as follows:
//!
//! ```text
//! $ RUST_LOG="[model{name=kettle}]=warn" cargo run --release my_simulation
//! [2001-01-01 00:00:06.000000000]  WARN model{name="kettle"}: my_simulation: water is boiling
//! [2001-01-01 00:01:36.000000000]  WARN model{name="kettle"}: my_simulation: water is ready
//! ```
//!
//! If the `model` span name collides with that of spans defined outside
//! `nexosim`, the above filters can be made more specific using
//! `nexosim[model]` instead of just `[model]`.
//!
//!
//! # Customization
//!
//! The [`tracing-subscriber`][tracing_subscriber] crate allows for
//! customization such as logging to files or formatting logs with JSON.
//!
//! Further customization is possible by implementing a
//! [`tracing_subscriber::layer::Layer`] or a dedicated [`tracing::Subscriber`].

use std::fmt;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};

use crate::executor::SIMULATION_CONTEXT;

/// A timer that can be used in conjunction with the
/// [`tracing-subscriber`][tracing_subscriber] crate to log events using the
/// simulation time instead of (or on top of) the wall clock time.
///
/// See the [module-level documentation][crate::tracing] for more details.
#[derive(Default, Debug)]
pub struct SimulationTime<const VERBOSE: bool, T> {
    sys_timer: T,
}

impl SimulationTime<false, SystemTime> {
    /// Constructs a new simulation timer which falls back to the [`SystemTime`]
    /// timer for events generated outside the simulator.
    pub fn with_system_timer() -> Self {
        Self::default()
    }
}

impl SimulationTime<true, SystemTime> {
    /// Constructs a new simulation timer which prepends a [`SystemTime`]
    /// timestamp to all tracing events, as well as a simulation timestamp for
    /// simulation events.
    pub fn with_system_timer_always() -> Self {
        Self::default()
    }
}

impl<T: FormatTime> SimulationTime<false, T> {
    /// Constructs a new simulation timer which falls back to the provided
    /// timer for tracing events generated outside the simulator.
    pub fn with_custom_timer(sys_timer: T) -> Self {
        Self { sys_timer }
    }
}

impl<T: FormatTime> SimulationTime<true, T> {
    /// Constructs a new simulation timer which prepends a timestamp generated
    /// with the provided timer to all tracing events, as well as a simulation
    /// timestamp for simulation events.
    pub fn with_custom_timer_always(sys_timer: T) -> Self {
        Self { sys_timer }
    }
}

impl<const VERBOSE: bool, T: FormatTime> FormatTime for SimulationTime<VERBOSE, T> {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        SIMULATION_CONTEXT
            .map(|ctx| {
                if VERBOSE {
                    self.sys_timer.format_time(w)?;
                    w.write_char(' ')?;
                }
                write!(w, "[{:.9}]", ctx.time_reader.try_read().unwrap())
            })
            .unwrap_or_else(|| self.sys_timer.format_time(w))
    }
}
