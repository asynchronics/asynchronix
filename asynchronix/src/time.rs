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
//!     pub fn set(&mut self, setting: MonotonicTime, cx: &mut Context<Self>) {
//!         if cx.schedule_event(setting, Self::ring, ()).is_err() {
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

pub(crate) type AtomicTime = crate::util::sync_cell::SyncCell<TearableAtomicTime>;
pub(crate) type AtomicTimeReader = crate::util::sync_cell::SyncCellReader<TearableAtomicTime>;

/// Trait abstracting over time-absolute and time-relative deadlines.
///
/// This trait is implemented by [`std::time::Duration`] and
/// [`MonotonicTime`].
pub trait Deadline {
    /// Make this deadline into an absolute timestamp, using the provided
    /// current time as a reference.
    fn into_time(self, now: MonotonicTime) -> MonotonicTime;
}

impl Deadline for std::time::Duration {
    #[inline(always)]
    fn into_time(self, now: MonotonicTime) -> MonotonicTime {
        now + self
    }
}

impl Deadline for MonotonicTime {
    #[inline(always)]
    fn into_time(self, _: MonotonicTime) -> MonotonicTime {
        self
    }
}
