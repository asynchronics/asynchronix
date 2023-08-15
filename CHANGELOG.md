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
