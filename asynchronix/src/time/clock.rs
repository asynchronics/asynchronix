use std::time::{Duration, Instant, SystemTime};

use crate::time::MonotonicTime;

/// A type that can be used to synchronize a simulation.
///
/// This trait abstract over the different types of clocks, such as
/// as-fast-as-possible and real-time clocks.
///
/// A clock can be associated to a simulation at initialization time by calling
/// [`SimInit::init_with_clock()`](crate::simulation::SimInit::init_with_clock).
pub trait Clock {
    /// Blocks until the deadline.
    fn synchronize(&mut self, deadline: MonotonicTime) -> SyncStatus;
}

/// The current synchronization status of a clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SyncStatus {
    /// The clock is synchronized.
    Synchronized,
    /// The clock is lagging behind by the specified offset.
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
pub struct SystemClock {
    wall_clock_ref: Instant,
    simulation_ref: MonotonicTime,
}

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
    /// let t0 = MonotonicTime::new(1_234_567_890, 0);
    ///
    /// // Make the simulation start in 1s.
    /// let clock = SystemClock::from_instant(t0, Instant::now() + Duration::from_secs(1));
    ///
    /// let simu = SimInit::new()
    /// //  .add_model(...)
    /// //  .add_model(...)
    ///     .init_with_clock(t0, clock);
    /// ```
    pub fn from_instant(simulation_ref: MonotonicTime, wall_clock_ref: Instant) -> Self {
        Self {
            wall_clock_ref,
            simulation_ref,
        }
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
    /// let t0 = MonotonicTime::new(1_234_567_890, 0);
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
    ///     .init_with_clock(t0, clock);
    /// ```
    pub fn from_system_time(simulation_ref: MonotonicTime, wall_clock_ref: SystemTime) -> Self {
        // Select the best-correlated `Instant`/`SystemTime` pair from several
        // samples to improve robustness towards possible thread suspension
        // between the calls to `SystemTime::now()` and `Instant::now()`.
        const SAMPLES: usize = 3;

        let mut last_instant = Instant::now();
        let mut min_delta = Duration::MAX;
        let mut ref_time = None;

        // Select the best-correlated instant/date pair.
        for _ in 0..SAMPLES {
            // The inner loop is to work around monotonic clock platform bugs
            // that may cause `checked_duration_since` to fail.
            let (date, instant, delta) = loop {
                let date = SystemTime::now();
                let instant = Instant::now();
                let delta = instant.checked_duration_since(last_instant);
                last_instant = instant;

                if let Some(delta) = delta {
                    break (date, instant, delta);
                }
            };

            // Store the current instant/date if the time elapsed since the last
            // measurement is shorter than the previous candidate.
            if min_delta > delta {
                min_delta = delta;
                ref_time = Some((instant, date));
            }
        }

        // Set the selected instant/date as the wall clock reference and adjust
        // the simulation reference accordingly.
        let (instant_ref, date_ref) = ref_time.unwrap();
        let simulation_ref = if date_ref > wall_clock_ref {
            let correction = date_ref.duration_since(wall_clock_ref).unwrap();

            simulation_ref + correction
        } else {
            let correction = wall_clock_ref.duration_since(date_ref).unwrap();

            simulation_ref - correction
        };

        Self {
            wall_clock_ref: instant_ref,
            simulation_ref,
        }
    }
}

impl Clock for SystemClock {
    /// Blocks until the system time corresponds to the specified simulation
    /// time.
    fn synchronize(&mut self, deadline: MonotonicTime) -> SyncStatus {
        let target_time = if deadline >= self.simulation_ref {
            self.wall_clock_ref + deadline.duration_since(self.simulation_ref)
        } else {
            self.wall_clock_ref - self.simulation_ref.duration_since(deadline)
        };

        let now = Instant::now();

        match target_time.checked_duration_since(now) {
            Some(sleep_duration) => {
                spin_sleep::sleep(sleep_duration);

                SyncStatus::Synchronized
            }
            None => SyncStatus::OutOfSync(now.duration_since(target_time)),
        }
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
