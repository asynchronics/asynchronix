# 0.2.4 (2024-11-16)

- Add crate rename notice

# 0.2.3 (2024-08-24)

- Force the waker VTable to be uniquely instantiated to re-enable the
  `will_wake` optimisation after its implementation was changed in `std` ([#38])
- Ignore broadcast error when sending to a closed `EventStream` ([#37])

[#37]: https://github.com/asynchronics/asynchronix/pull/37
[#38]: https://github.com/asynchronics/asynchronix/pull/38


# 0.2.2 (2024-04-04)

- Add `serde` feature and serialization support for `MonotonicTime` ([#19]).
- Update `multishot` dependency due to soundness issue in older version ([#23]).

[#19]: https://github.com/asynchronics/asynchronix/pull/19
[#23]: https://github.com/asynchronics/asynchronix/pull/23

# 0.2.1 (2024-03-06)

### Added

- Add support for custom clocks and provide an optional real-time clock
  ([#9], [#15]).

[#9]: https://github.com/asynchronics/asynchronix/pull/9
[#15]: https://github.com/asynchronics/asynchronix/pull/15

### Misc

- Update copyright in MIT license to include contributors.

# 0.2.0 (2023-08-15)

### Added (API-breaking changes)

- Enable cancellation of events up to the very last moment, even if the event is
  scheduled for the current time ([#5]).
- Makes it possible to schedule periodic events from a `Simulation` or a model's
  `Scheduler` ([#6]).
- Mitigate the increase in API surface by merging each pair of
  `schedule_*event_in`/`schedule_*event_at` methods into one overloaded
  `schedule_*event` method that accept either a `Duration` or a `MonotonicTime`
  ([#7]).

[#5]: https://github.com/asynchronics/asynchronix/pull/5
[#6]: https://github.com/asynchronics/asynchronix/pull/6
[#7]: https://github.com/asynchronics/asynchronix/pull/7


# 0.1.0 (2023-01-16)

Initial release
