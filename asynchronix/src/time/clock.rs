use std::time::{Duration, Instant, SystemTime};

use tai_time::MonotonicClock;

use crate::time::MonotonicTime;

/// A type that can be used to synchronize a simulation.
///
/// This trait abstracts over different types of clocks, such as
/// as-fast-as-possible and real-time clocks.
///
/// A clock can be associated to a simulation prior to initialization by calling
/// [`SimInit::set_clock()`](crate::simulation::SimInit::set_clock).
pub trait Clock: Send {
    /// Blocks until the deadline.
    fn synchronize(&mut self, deadline: MonotonicTime) -> SyncStatus;
}

/// The current synchronization status of a clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SyncStatus {
    /// The clock is synchronized.
    Synchronized,
    /// The deadline has already elapsed and lags behind the current clock time
    /// by the duration given in the payload.
    OutOfSync(Duration),
}

/// A dummy [`Clock`] that ignores synchronization.
///
/// Choosing this clock effectively makes the simulation run as fast as
/// possible.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoClock {}

impl NoClock {
    /// Constructs a new `NoClock` object.
    pub fn new() -> Self {
        Self {}
    }
}

impl Clock for NoClock {
    /// Returns immediately with status `SyncStatus::Synchronized`.
    fn synchronize(&mut self, _: MonotonicTime) -> SyncStatus {
        SyncStatus::Synchronized
    }
}

/// A real-time [`Clock`] based on the system's monotonic clock.
///
/// This clock accepts an arbitrary reference time and remains synchronized with
/// the system's monotonic clock.
#[derive(Copy, Clone, Debug)]
pub struct SystemClock(MonotonicClock);

impl SystemClock {
    /// Constructs a `SystemClock` with an offset between simulation clock and
    /// wall clock specified by a simulation time matched to an [`Instant`]
    /// timestamp.
    ///
    /// The provided reference time may lie in the past or in the future.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    ///
    /// use asynchronix::simulation::SimInit;
    /// use asynchronix::time::{MonotonicTime, SystemClock};
    ///
    /// let t0 = MonotonicTime::new(1_234_567_890, 0).unwrap();
    ///
    /// // Make the simulation start in 1s.
    /// let clock = SystemClock::from_instant(t0, Instant::now() + Duration::from_secs(1));
    ///
    /// let simu = SimInit::new()
    /// //  .add_model(...)
    /// //  .add_model(...)
    ///     .set_clock(clock)
    ///     .init(t0);
    /// ```
    pub fn from_instant(simulation_ref: MonotonicTime, wall_clock_ref: Instant) -> Self {
        Self(MonotonicClock::init_from_instant(
            simulation_ref,
            wall_clock_ref,
        ))
    }

    /// Constructs a `SystemClock` with an offset between simulation clock and
    /// wall clock specified by a simulation time matched to a [`SystemTime`]
    /// timestamp.
    ///
    /// The provided reference time may lie in the past or in the future.
    ///
    /// Note that, even though the wall clock reference is specified with the
    /// (non-monotonic) system clock, the [`synchronize()`](Clock::synchronize)
    /// method will still use the system's _monotonic_ clock. This constructor
    /// makes a best-effort attempt at synchronizing the monotonic clock with
    /// the non-monotonic system clock _at construction time_, but this
    /// synchronization will be lost if the system clock is subsequently
    /// modified through administrative changes, introduction of leap second or
    /// otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, UNIX_EPOCH};
    ///
    /// use asynchronix::simulation::SimInit;
    /// use asynchronix::time::{MonotonicTime, SystemClock};
    ///
    /// let t0 = MonotonicTime::new(1_234_567_890, 0).unwrap();
    ///
    /// // Make the simulation start at the next full second boundary.
    /// let now_secs = UNIX_EPOCH.elapsed().unwrap().as_secs();
    /// let start_time = UNIX_EPOCH + Duration::from_secs(now_secs + 1);
    ///
    /// let clock = SystemClock::from_system_time(t0, start_time);
    ///
    /// let simu = SimInit::new()
    /// //  .add_model(...)
    /// //  .add_model(...)
    ///     .set_clock(clock)
    ///     .init(t0);
    /// ```
    pub fn from_system_time(simulation_ref: MonotonicTime, wall_clock_ref: SystemTime) -> Self {
        Self(MonotonicClock::init_from_system_time(
            simulation_ref,
            wall_clock_ref,
        ))
    }
}

impl Clock for SystemClock {
    /// Blocks until the system time corresponds to the specified simulation
    /// time.
    fn synchronize(&mut self, deadline: MonotonicTime) -> SyncStatus {
        let now = self.0.now();
        if now <= deadline {
            spin_sleep::sleep(deadline.duration_since(now));

            return SyncStatus::Synchronized;
        }

        SyncStatus::OutOfSync(now.duration_since(deadline))
    }
}

/// An automatically initialized real-time [`Clock`] based on the system's
/// monotonic clock.
///
/// This clock is similar to [`SystemClock`] except that the first call to
/// [`synchronize()`](Clock::synchronize) never blocks and implicitly defines
/// the reference time. In other words, the clock starts running on its first
/// invocation.
#[derive(Copy, Clone, Debug, Default)]
pub struct AutoSystemClock {
    inner: Option<SystemClock>,
}

impl AutoSystemClock {
    /// Constructs a new `AutoSystemClock`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clock for AutoSystemClock {
    /// Initializes the time reference and returns immediately on the first
    /// call, otherwise blocks until the system time corresponds to the
    /// specified simulation time.
    fn synchronize(&mut self, deadline: MonotonicTime) -> SyncStatus {
        match &mut self.inner {
            None => {
                let now = Instant::now();
                self.inner = Some(SystemClock::from_instant(deadline, now));

                SyncStatus::Synchronized
            }
            Some(clock) => clock.synchronize(deadline),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn smoke_system_clock() {
        let t0 = MonotonicTime::EPOCH;
        const TOLERANCE: f64 = 0.0005; // [s]

        let now = Instant::now();
        let mut clock = SystemClock::from_instant(t0, now);
        let t1 = t0 + Duration::from_millis(200);
        clock.synchronize(t1);
        let elapsed = now.elapsed().as_secs_f64();
        let dt = t1.duration_since(t0).as_secs_f64();

        assert!(
            (dt - elapsed) <= TOLERANCE,
            "Expected t = {:.6}s +/- {:.6}s, measured t = {:.6}s",
            dt,
            TOLERANCE,
            elapsed,
        );
    }
}
