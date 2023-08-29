//! Monotonic simulation time.

use std::error::Error;
use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use crate::util::sync_cell::TearableAtomic;

const NANOS_PER_SEC: u32 = 1_000_000_000;

/// A nanosecond-precision monotonic clock timestamp.
///
/// A timestamp specifies a [TAI] point in time. It is represented as a 64-bit
/// signed number of seconds and a positive number of nanoseconds, counted with
/// reference to 1970-01-01 00:00:00 TAI. This timestamp format has a number of
/// desirable properties:
///
/// - it enables cheap inter-operation with the standard [`Duration`] type which
///   uses a very similar internal representation,
/// - it constitutes a strict 96-bit superset of 80-bit PTP IEEE-1588
///   timestamps, with the same epoch,
/// - if required, exact conversion to a Unix timestamp is trivial and only
///   requires subtracting from this timestamp the number of leap seconds
///   between TAI and UTC time (see also the
///   [`as_unix_secs()`](MonotonicTime::as_unix_secs) method).
///
/// Although no date-time conversion methods are provided, conversion from
/// timestamp to TAI date-time representations and back can be easily performed
/// using `NaiveDateTime` from the [chrono] crate or `OffsetDateTime` from the
/// [time] crate, treating the timestamp as a regular (UTC) Unix timestamp.
///
/// [TAI]: https://en.wikipedia.org/wiki/International_Atomic_Time
/// [chrono]: https://crates.io/crates/chrono
/// [time]: https://crates.io/crates/time
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use asynchronix::time::MonotonicTime;
///
/// // Set the timestamp to 2009-02-13 23:31:30.987654321 TAI.
/// let mut timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
///
/// // Increment the timestamp by 123.456s.
/// timestamp += Duration::new(123, 456_000_000);
///
/// assert_eq!(timestamp, MonotonicTime::new(1_234_568_014, 443_654_321));
/// assert_eq!(timestamp.as_secs(), 1_234_568_014);
/// assert_eq!(timestamp.subsec_nanos(), 443_654_321);
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonotonicTime {
    /// The number of whole seconds in the future (if positive) or in the past
    /// (if negative) of 1970-01-01 00:00:00 TAI.
    ///
    /// Note that the automatic derivation of `PartialOrd` relies on
    /// lexicographical comparison so the `secs` field must appear before
    /// `nanos` in declaration order to be given higher priority.
    secs: i64,
    /// The sub-second number of nanoseconds in the future of the point in time
    /// defined by `secs`.
    nanos: u32,
}

impl MonotonicTime {
    /// The epoch used by `MonotonicTime`, equal to 1970-01-01 00:00:00 TAI.
    ///
    /// This epoch coincides with the PTP epoch defined in the IEEE-1588
    /// standard.
    pub const EPOCH: Self = Self { secs: 0, nanos: 0 };

    /// The minimum possible `MonotonicTime` timestamp.
    pub const MIN: Self = Self {
        secs: i64::MIN,
        nanos: 0,
    };

    /// The maximum possible `MonotonicTime` timestamp.
    pub const MAX: Self = Self {
        secs: i64::MAX,
        nanos: NANOS_PER_SEC - 1,
    };

    /// Creates a timestamp directly from timestamp parts.
    ///
    /// The number of seconds is relative to the [`EPOCH`](MonotonicTime::EPOCH)
    /// (1970-01-01 00:00:00 TAI). It is negative for dates in the past of the
    /// epoch.
    ///
    /// The number of nanoseconds is always positive and always points towards
    /// the future.
    ///
    /// # Panics
    ///
    /// This constructor will panic if the number of nanoseconds is greater than
    /// or equal to 1 second.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // A timestamp set to 2009-02-13 23:31:30.987654321 TAI.
    /// let timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
    ///
    /// // A timestamp set 3.5s before the epoch.
    /// let timestamp = MonotonicTime::new(-4, 500_000_000);
    /// assert_eq!(timestamp, MonotonicTime::EPOCH - Duration::new(3, 500_000_000));
    /// ```
    pub const fn new(secs: i64, subsec_nanos: u32) -> Self {
        assert!(
            subsec_nanos < NANOS_PER_SEC,
            "invalid number of nanoseconds"
        );

        Self {
            secs,
            nanos: subsec_nanos,
        }
    }

    /// Creates a timestamp from the current system time.
    ///
    /// The argument is the current difference between TAI and UTC time in
    /// seconds (a.k.a. leap seconds). For reference, this offset has been +37s
    /// since 2017-01-01, a value which is to remain valid until at least
    /// 2024-06-29. See the [official IERS bulletin
    /// C](https://datacenter.iers.org/data/latestVersion/bulletinC.txt) for
    /// leap second announcements or the [IETF
    /// table](https://www.ietf.org/timezones/data/leap-seconds.list) for
    /// current and historical values.
    ///
    /// # Errors
    ///
    /// This method will return an error if the reported system time is in the
    /// past of the Unix epoch or if the offset-adjusted timestamp is outside
    /// the representable range.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // Compute the current TAI time assuming that the current difference
    /// // between TAI and UTC time is 37s.
    /// let timestamp = MonotonicTime::from_system(37).unwrap();
    /// ```
    pub fn from_system(leap_secs: i64) -> Result<Self, SystemTimeError> {
        let utc_timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| SystemTimeError::InvalidSystemTime)?;

        Self::new(leap_secs, 0)
            .checked_add(utc_timestamp)
            .ok_or(SystemTimeError::OutOfRange)
    }

    /// Returns the number of whole seconds relative to
    /// [`EPOCH`](MonotonicTime::EPOCH) (1970-01-01 00:00:00 TAI).
    ///
    /// Consistently with the interpretation of seconds and nanoseconds in the
    /// [`new()`](Self::new) constructor, seconds are always rounded towards
    /// `-∞`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
    /// assert_eq!(timestamp.as_secs(), 1_234_567_890);
    ///
    /// let timestamp = MonotonicTime::EPOCH - Duration::new(3, 500_000_000);
    /// assert_eq!(timestamp.as_secs(), -4);
    /// ```
    pub const fn as_secs(&self) -> i64 {
        self.secs
    }

    /// Returns the number of seconds of the corresponding Unix time.
    ///
    /// The argument is the difference between TAI and UTC time in seconds
    /// (a.k.a. leap seconds) applicable at the date represented by the
    /// timestamp. See the [official IERS bulletin
    /// C](https://datacenter.iers.org/data/latestVersion/bulletinC.txt) for
    /// leap second announcements or the [IETF
    /// table](https://www.ietf.org/timezones/data/leap-seconds.list) for
    /// current and historical values.
    ///
    /// This method merely subtracts the offset from the value returned by
    /// [`as_secs()`](Self::as_secs) and checks for potential overflow; its main
    /// purpose is to prevent mistakes regarding the direction in which the
    /// offset should be applied.
    ///
    /// Note that the nanosecond part of a Unix timestamp can be simply
    /// retrieved with [`subsec_nanos()`](Self::subsec_nanos) since UTC and TAI
    /// differ by a whole number of seconds.
    ///
    /// # Panics
    ///
    /// This will panic if the offset-adjusted timestamp cannot be represented
    /// as an `i64`.
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::time::MonotonicTime;
    ///
    /// // Set the date to 2000-01-01 00:00:00 TAI.
    /// let timestamp = MonotonicTime::new(946_684_800, 0);
    ///
    /// // Convert to a Unix timestamp, accounting for the +32s difference between
    /// // TAI and UTC on 2000-01-01.
    /// let unix_secs = timestamp.as_unix_secs(32);
    /// ```
    pub const fn as_unix_secs(&self, leap_secs: i64) -> i64 {
        if let Some(secs) = self.secs.checked_sub(leap_secs) {
            secs
        } else {
            panic!("timestamp outside representable range");
        }
    }

    /// Returns the sub-second fractional part in nanoseconds.
    ///
    /// Note that nanoseconds always point towards the future even if the date
    /// is in the past of the [`EPOCH`](MonotonicTime::EPOCH).
    ///
    /// # Examples
    ///
    /// ```
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
    /// assert_eq!(timestamp.subsec_nanos(), 987_654_321);
    /// ```
    pub const fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Adds a duration to a timestamp, checking for overflow.
    ///
    /// Returns `None` if overflow occurred.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
    /// assert!(timestamp.checked_add(Duration::new(10, 123_456_789)).is_some());
    /// assert!(timestamp.checked_add(Duration::MAX).is_none());
    /// ```
    pub const fn checked_add(self, rhs: Duration) -> Option<Self> {
        // A durations in seconds greater than `i64::MAX` is actually fine as
        // long as the number of seconds does not effectively overflow which is
        // why the below does not use `checked_add`. So technically the below
        // addition may wrap around on the negative side due to the
        // unsigned-to-signed cast of the duration, but this does not
        // necessarily indicate an actual overflow. Actual overflow can be ruled
        // out by verifying that the new timestamp is in the future of the old
        // timestamp.
        let mut secs = self.secs.wrapping_add(rhs.as_secs() as i64);

        // Check for overflow.
        if secs < self.secs {
            return None;
        }

        let mut nanos = self.nanos + rhs.subsec_nanos();
        if nanos >= NANOS_PER_SEC {
            secs = if let Some(s) = secs.checked_add(1) {
                s
            } else {
                return None;
            };
            nanos -= NANOS_PER_SEC;
        }

        Some(Self { secs, nanos })
    }

    /// Subtracts a duration from a timestamp, checking for overflow.
    ///
    /// Returns `None` if overflow occurred.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp = MonotonicTime::new(1_234_567_890, 987_654_321);
    /// assert!(timestamp.checked_sub(Duration::new(10, 123_456_789)).is_some());
    /// assert!(timestamp.checked_sub(Duration::MAX).is_none());
    /// ```
    pub const fn checked_sub(self, rhs: Duration) -> Option<Self> {
        // A durations in seconds greater than `i64::MAX` is actually fine as
        // long as the number of seconds does not effectively overflow, which is
        // why the below does not use `checked_sub`. So technically the below
        // subtraction may wrap around on the positive side due to the
        // unsigned-to-signed cast of the duration, but this does not
        // necessarily indicate an actual overflow. Actual overflow can be ruled
        // out by verifying that the new timestamp is in the past of the old
        // timestamp.
        let mut secs = self.secs.wrapping_sub(rhs.as_secs() as i64);

        // Check for overflow.
        if secs > self.secs {
            return None;
        }

        let nanos = if self.nanos < rhs.subsec_nanos() {
            secs = if let Some(s) = secs.checked_sub(1) {
                s
            } else {
                return None;
            };

            (self.nanos + NANOS_PER_SEC) - rhs.subsec_nanos()
        } else {
            self.nanos - rhs.subsec_nanos()
        };

        Some(Self { secs, nanos })
    }

    /// Subtracts a timestamp from another timestamp.
    ///
    /// # Panics
    ///
    /// Panics if the argument lies in the future of `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp_earlier = MonotonicTime::new(1_234_567_879, 987_654_321);
    /// let timestamp_later = MonotonicTime::new(1_234_567_900, 123_456_789);
    /// assert_eq!(
    ///     timestamp_later.duration_since(timestamp_earlier),
    ///     Duration::new(20, 135_802_468)
    /// );
    /// ```
    pub fn duration_since(self, earlier: Self) -> Duration {
        self.checked_duration_since(earlier)
            .expect("attempt to substract a timestamp from an earlier timestamp")
    }

    /// Computes the duration elapsed between a timestamp and an earlier
    /// timestamp, checking that the timestamps are appropriately ordered.
    ///
    /// Returns `None` if the argument lies in the future of `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use asynchronix::time::MonotonicTime;
    ///
    /// let timestamp_earlier = MonotonicTime::new(1_234_567_879, 987_654_321);
    /// let timestamp_later = MonotonicTime::new(1_234_567_900, 123_456_789);
    /// assert!(timestamp_later.checked_duration_since(timestamp_earlier).is_some());
    /// assert!(timestamp_earlier.checked_duration_since(timestamp_later).is_none());
    /// ```
    pub const fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        // If the subtraction of the nanosecond fractions would overflow, carry
        // over one second to the nanoseconds.
        let (secs, nanos) = if earlier.nanos > self.nanos {
            if let Some(s) = self.secs.checked_sub(1) {
                (s, self.nanos + NANOS_PER_SEC)
            } else {
                return None;
            }
        } else {
            (self.secs, self.nanos)
        };

        // Make sure the computation of the duration will not overflow the
        // seconds.
        if secs < earlier.secs {
            return None;
        }

        // This subtraction may wrap around if the difference between the two
        // timestamps is more than `i64::MAX`, but even if it does the result
        // will be correct once cast to an unsigned integer.
        let delta_secs = secs.wrapping_sub(earlier.secs) as u64;

        // The below subtraction is guaranteed to never overflow.
        let delta_nanos = nanos - earlier.nanos;

        Some(Duration::new(delta_secs, delta_nanos))
    }
}

impl Add<Duration> for MonotonicTime {
    type Output = Self;

    /// Adds a duration to a timestamp.
    ///
    /// # Panics
    ///
    /// This function panics if the resulting timestamp cannot be
    /// represented. See [`MonotonicTime::checked_add`] for a panic-free
    /// version.
    fn add(self, other: Duration) -> Self {
        self.checked_add(other)
            .expect("overflow when adding duration to timestamp")
    }
}

impl Sub<Duration> for MonotonicTime {
    type Output = Self;

    /// Subtracts a duration from a timestamp.
    ///
    /// # Panics
    ///
    /// This function panics if the resulting timestamp cannot be
    /// represented. See [`MonotonicTime::checked_sub`] for a panic-free
    /// version.
    fn sub(self, other: Duration) -> Self {
        self.checked_sub(other)
            .expect("overflow when subtracting duration from timestamp")
    }
}

impl AddAssign<Duration> for MonotonicTime {
    /// Increments the timestamp by a duration.
    ///
    /// # Panics
    ///
    /// This function panics if the resulting timestamp cannot be represented.
    fn add_assign(&mut self, other: Duration) {
        *self = *self + other;
    }
}

impl SubAssign<Duration> for MonotonicTime {
    /// Decrements the timestamp by a duration.
    ///
    /// # Panics
    ///
    /// This function panics if the resulting timestamp cannot be represented.
    fn sub_assign(&mut self, other: Duration) {
        *self = *self - other;
    }
}

/// An error that may be returned when initializing a [`MonotonicTime`] from
/// system time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SystemTimeError {
    /// The system time is in the past of the Unix epoch.
    InvalidSystemTime,
    /// The system time cannot be represented as a `MonotonicTime`.
    OutOfRange,
}

impl fmt::Display for SystemTimeError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSystemTime => write!(fmt, "invalid system time"),
            Self::OutOfRange => write!(fmt, "timestamp outside representable range"),
        }
    }
}

impl Error for SystemTimeError {}

/// A tearable atomic adapter over a `MonotonicTime`.
///
/// This makes it possible to store the simulation time in a `SyncCell`, an
/// efficient, seqlock-based alternative to `RwLock`.
pub(crate) struct TearableAtomicTime {
    secs: AtomicI64,
    nanos: AtomicU32,
}

impl TearableAtomicTime {
    pub(crate) fn new(time: MonotonicTime) -> Self {
        Self {
            secs: AtomicI64::new(time.secs),
            nanos: AtomicU32::new(time.nanos),
        }
    }
}

impl TearableAtomic for TearableAtomicTime {
    type Value = MonotonicTime;

    fn tearable_load(&self) -> MonotonicTime {
        // Load each field separately. This can never create invalid values of a
        // `MonotonicTime`, even if the load is torn.
        MonotonicTime {
            secs: self.secs.load(Ordering::Relaxed),
            nanos: self.nanos.load(Ordering::Relaxed),
        }
    }

    fn tearable_store(&self, value: MonotonicTime) {
        // Write each field separately. This can never create invalid values of
        // a `MonotonicTime`, even if the store is torn.
        self.secs.store(value.secs, Ordering::Relaxed);
        self.nanos.store(value.nanos, Ordering::Relaxed);
    }
}

#[cfg(all(test, not(asynchronix_loom)))]
mod tests {
    use super::*;

    #[test]
    fn time_equality() {
        let t0 = MonotonicTime::new(123, 123_456_789);
        let t1 = MonotonicTime::new(123, 123_456_789);
        let t2 = MonotonicTime::new(123, 123_456_790);
        let t3 = MonotonicTime::new(124, 123_456_789);

        assert_eq!(t0, t1);
        assert_ne!(t0, t2);
        assert_ne!(t0, t3);
    }

    #[test]
    fn time_ordering() {
        let t0 = MonotonicTime::new(0, 1);
        let t1 = MonotonicTime::new(1, 0);

        assert!(t1 > t0);
    }

    #[cfg(not(miri))]
    #[test]
    fn time_from_system_smoke() {
        const START_OF_2022: i64 = 1640995200;
        const START_OF_2050: i64 = 2524608000;

        let now_secs = MonotonicTime::from_system(0).unwrap().as_secs();

        assert!(now_secs > START_OF_2022);
        assert!(now_secs < START_OF_2050);
    }

    #[test]
    #[should_panic]
    fn time_invalid() {
        MonotonicTime::new(123, 1_000_000_000);
    }

    #[test]
    fn time_duration_since_smoke() {
        let t0 = MonotonicTime::new(100, 100_000_000);
        let t1 = MonotonicTime::new(123, 223_456_789);

        assert_eq!(
            t1.checked_duration_since(t0),
            Some(Duration::new(23, 123_456_789))
        );
    }

    #[test]
    fn time_duration_with_carry() {
        let t0 = MonotonicTime::new(100, 200_000_000);
        let t1 = MonotonicTime::new(101, 100_000_000);

        assert_eq!(
            t1.checked_duration_since(t0),
            Some(Duration::new(0, 900_000_000))
        );
    }

    #[test]
    fn time_duration_since_extreme() {
        const MIN_TIME: MonotonicTime = MonotonicTime::new(i64::MIN, 0);
        const MAX_TIME: MonotonicTime = MonotonicTime::new(i64::MAX, NANOS_PER_SEC - 1);

        assert_eq!(
            MAX_TIME.checked_duration_since(MIN_TIME),
            Some(Duration::new(u64::MAX, NANOS_PER_SEC - 1))
        );
    }

    #[test]
    fn time_duration_since_invalid() {
        let t0 = MonotonicTime::new(100, 0);
        let t1 = MonotonicTime::new(99, 0);

        assert_eq!(t1.checked_duration_since(t0), None);
    }

    #[test]
    fn time_add_duration_smoke() {
        let t = MonotonicTime::new(-100, 100_000_000);
        let dt = Duration::new(400, 300_000_000);

        assert_eq!(t + dt, MonotonicTime::new(300, 400_000_000));
    }

    #[test]
    fn time_add_duration_with_carry() {
        let t = MonotonicTime::new(-100, 900_000_000);
        let dt1 = Duration::new(400, 100_000_000);
        let dt2 = Duration::new(400, 300_000_000);

        assert_eq!(t + dt1, MonotonicTime::new(301, 0));
        assert_eq!(t + dt2, MonotonicTime::new(301, 200_000_000));
    }

    #[test]
    fn time_add_duration_extreme() {
        let t = MonotonicTime::new(i64::MIN, 0);
        let dt = Duration::new(u64::MAX, NANOS_PER_SEC - 1);

        assert_eq!(t + dt, MonotonicTime::new(i64::MAX, NANOS_PER_SEC - 1));
    }

    #[test]
    #[should_panic]
    fn time_add_duration_overflow() {
        let t = MonotonicTime::new(i64::MIN, 1);
        let dt = Duration::new(u64::MAX, NANOS_PER_SEC - 1);

        let _ = t + dt;
    }

    #[test]
    fn time_sub_duration_smoke() {
        let t = MonotonicTime::new(100, 500_000_000);
        let dt = Duration::new(400, 300_000_000);

        assert_eq!(t - dt, MonotonicTime::new(-300, 200_000_000));
    }

    #[test]
    fn time_sub_duration_with_carry() {
        let t = MonotonicTime::new(100, 100_000_000);
        let dt1 = Duration::new(400, 100_000_000);
        let dt2 = Duration::new(400, 300_000_000);

        assert_eq!(t - dt1, MonotonicTime::new(-300, 0));
        assert_eq!(t - dt2, MonotonicTime::new(-301, 800_000_000));
    }

    #[test]
    fn time_sub_duration_extreme() {
        let t = MonotonicTime::new(i64::MAX, NANOS_PER_SEC - 1);
        let dt = Duration::new(u64::MAX, NANOS_PER_SEC - 1);

        assert_eq!(t - dt, MonotonicTime::new(i64::MIN, 0));
    }

    #[test]
    #[should_panic]
    fn time_sub_duration_overflow() {
        let t = MonotonicTime::new(i64::MAX, NANOS_PER_SEC - 2);
        let dt = Duration::new(u64::MAX, NANOS_PER_SEC - 1);

        let _ = t - dt;
    }
}
