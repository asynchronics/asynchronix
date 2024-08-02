//! Simulation time and scheduling.
//!
//! This module provides most notably:
//!
//! * [`MonotonicTime`]: a monotonic timestamp based on the [TAI] time standard,
//! * [`Clock`]: a trait for types that can synchronize a simulation,
//!   implemented for instance by [`SystemClock`] and [`AutoSystemClock`].
//!
//! [TAI]: https://en.wikipedia.org/wiki/International_Atomic_Time
//!
//!
//! # Examples
//!
//! An alarm clock model that prints a message when the simulation time reaches
//! the specified timestamp.
//!
//! ```
//! use asynchronix::model::{Context, Model};
//! use asynchronix::time::MonotonicTime;
//!
//! // An alarm clock model.
//! pub struct AlarmClock {
//!     msg: String
//! }
//!
//! impl AlarmClock {
//!     // Creates a new alarm clock.
//!     pub fn new(msg: String) -> Self {
//!         Self { msg }
//!     }
//!
//!     // Sets an alarm [input port].
//!     pub fn set(&mut self, setting: MonotonicTime, context: &Context<Self>) {
//!         if context.scheduler.schedule_event(setting, Self::ring, ()).is_err() {
//!             println!("The alarm clock can only be set for a future time");
//!         }
//!     }
//!
//!     // Rings the alarm [private input port].
//!     fn ring(&mut self) {
//!         println!("{}", self.msg);
//!     }
//! }
//!
//! impl Model for AlarmClock {}
//! ```

mod clock;
mod monotonic_time;

pub use tai_time::MonotonicTime;

pub use clock::{AutoSystemClock, Clock, NoClock, SyncStatus, SystemClock};
pub(crate) use monotonic_time::TearableAtomicTime;
