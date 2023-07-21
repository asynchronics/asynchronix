//! Simulation time and scheduling.
//!
//! This module provides most notably:
//!
//! * [`MonotonicTime`]: a monotonic timestamp based on the [TAI] time standard,
//! * [`Scheduler`]: a model-local handle to the global scheduler that can be
//!   used by models to schedule future actions onto themselves.
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
//! use asynchronix::model::Model;
//! use asynchronix::time::{MonotonicTime, Scheduler};
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
//!     pub fn set(&mut self, setting: MonotonicTime, scheduler: &Scheduler<Self>) {
//!         if scheduler.schedule_event_at(setting, Self::ring, ()).is_err() {
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

mod monotonic_time;
mod scheduler;

pub(crate) use monotonic_time::TearableAtomicTime;
pub use monotonic_time::{MonotonicTime, SystemTimeError};
pub(crate) use scheduler::{
    schedule_event_at_unchecked, schedule_keyed_event_at_unchecked, SchedulerQueue,
};
pub use scheduler::{EventKey, ScheduledTimeError, Scheduler};
