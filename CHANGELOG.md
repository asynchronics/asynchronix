# 0.3.0-beta.0 (2024-11-16)

This beta release features a very large number of improvements and API changes,
including: 

- Add a gRPC server for remote execution ([#12], [#24], [#25], [#26], [#29],
  [#43])
- Single-threaded executor supporting compilation to WebAssembly ([#24])
- Add support for the `tracing` crate ([#47])
- Make `Output`s and `Requestor`s `Clone`-able ([#30], [#48]) 
- Make the global `Scheduler` an owned `Clone`-able type ([#30]) 
- Add an automatically managed action key for scheduled actions/events ([#27])
- Enable connection of different input/output pairs with `map_connect()` methods
  on `Output` and `Requestor` ([#32])
- Streamline the creation of data buses (SPI, CAN, MIL-STD-1553, SpaceWire etc.)
  with `filter_map_connect()` methods on `Output` and `Requestor` ([#32])
- Implement deadlock detection ([#51])
- Streamline the builder pattern for models with a `ProtoModel` trait ([#54])
- Implement execution timeout ([#57]) 
- Return an error when a real-time simulation clock looses synchronization
  ([#58])
- Catch model panics and report them as errors ([#60])
- Provide additional ordering guaranties when using the global scheduler ([#62])
- Remove `LineId` line disconnection API ([#63])

[#12]: https://github.com/asynchronics/nexosim/pull/12
[#24]: https://github.com/asynchronics/nexosim/pull/24
[#25]: https://github.com/asynchronics/nexosim/pull/25
[#26]: https://github.com/asynchronics/nexosim/pull/26
[#27]: https://github.com/asynchronics/nexosim/pull/27
[#29]: https://github.com/asynchronics/nexosim/pull/29
[#30]: https://github.com/asynchronics/nexosim/pull/30
[#32]: https://github.com/asynchronics/nexosim/pull/32
[#43]: https://github.com/asynchronics/nexosim/pull/43
[#47]: https://github.com/asynchronics/nexosim/pull/47
[#48]: https://github.com/asynchronics/nexosim/pull/48
[#51]: https://github.com/asynchronics/nexosim/pull/51
[#54]: https://github.com/asynchronics/nexosim/pull/54
[#57]: https://github.com/asynchronics/nexosim/pull/57
[#58]: https://github.com/asynchronics/nexosim/pull/58
[#60]: https://github.com/asynchronics/nexosim/pull/60
[#62]: https://github.com/asynchronics/nexosim/pull/62
[#63]: https://github.com/asynchronics/nexosim/pull/63

# 0.2.3 (2024-08-24)

- Force the waker VTable to be uniquely instantiated to re-enable the
  `will_wake` optimisation after its implementation was changed in `std` ([#38])
- Ignore broadcast error when sending to a closed `EventStream` ([#37])

[#37]: https://github.com/asynchronics/nexosim/pull/37
[#38]: https://github.com/asynchronics/nexosim/pull/38

# 0.2.2 (2024-04-04)

- Add `serde` feature and serialization support for `MonotonicTime` ([#19]).
- Update `multishot` dependency due to soundness issue in older version ([#23]).

[#19]: https://github.com/asynchronics/nexosim/pull/19
[#23]: https://github.com/asynchronics/nexosim/pull/23

# 0.2.1 (2024-03-06)

### Added

- Add support for custom clocks and provide an optional real-time clock
  ([#9], [#15]).

[#9]: https://github.com/asynchronics/nexosim/pull/9
[#15]: https://github.com/asynchronics/nexosim/pull/15

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

[#5]: https://github.com/asynchronics/nexosim/pull/5
[#6]: https://github.com/asynchronics/nexosim/pull/6
[#7]: https://github.com/asynchronics/nexosim/pull/7


# 0.1.0 (2023-01-16)

Initial release
