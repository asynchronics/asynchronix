# NeXosim

NeXosim (né Asynchronix) is a developer-friendly, highly optimized
discrete-event simulation framework written in Rust. It is meant to scale from
small, simple simulations to very large simulation benches with complex
time-driven state machines.

[![Cargo](https://img.shields.io/crates/v/nexosim.svg)](https://crates.io/crates/nexosim)
[![Documentation](https://docs.rs/nexosim/badge.svg)](https://docs.rs/nexosim)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/asynchronics/nexosim#license)


## Overview

NeXosim is a simulator that leverages asynchronous programming to
transparently and efficiently auto-parallelize simulations by means of a custom
multi-threaded executor.

It promotes a component-oriented architecture that is familiar to system
engineers and closely resembles [flow-based programming][FBP]: a model is
essentially an isolated entity with a fixed set of typed inputs and outputs,
communicating with other models through message passing via connections defined
during bench assembly.

Although the main impetus for its development was the need for simulators able
to handle large cyberphysical systems, NeXosim is a general-purpose
discrete-event simulator expected to be suitable for a wide range of simulation
activities. It draws from experience on spacecraft real-time simulators but
differs from existing tools in the space industry in a number of respects,
including:

1) *performance*: by taking advantage of Rust's excellent support for
   multithreading and asynchronous programming, simulation models can run
   efficiently in parallel with all required synchronization being transparently
   handled by the simulator,
2) *developer-friendliness*: an ergonomic API and Rust's support for algebraic
   types make it ideal for the "cyber" part in cyberphysical, i.e. for modelling
   digital devices with even very complex state machines,
3) *open-source*: last but not least, NeXosim is distributed under the very
   permissive MIT and Apache 2 licenses, with the explicit intent to foster an
   ecosystem where models can be easily exchanged without reliance on
   proprietary APIs.

[FBP]: https://en.wikipedia.org/wiki/Flow-based_programming


## Documentation

The [API] documentation is relatively exhaustive and includes a practical
overview which should provide all necessary information to get started.

More fleshed out examples can also be found in the dedicated
[simulator](nexosim/examples) and [utilities](nexosim-util/examples)
directories.

[API]: https://docs.rs/nexosim


## Usage

Note that this page currently documents the latest beta version for the upcoming
`0.3.0` release, which contains numerous improvements over the `0.2` branch.
While the API is considered nearly frozen, some minor changes are still
possible.

To use the beta version, add to your `Cargo.toml`:

```toml
[dependencies]
nexosim = "0.3.0-beta.0"
```

If you would rather stay for now with the last official release (published under
the `asynchronix` name), add this to your `Cargo.toml`:

```toml
[dependencies]
asynchronix = "0.2.3"
```


## Example

```rust
// A system made of 2 identical models.
// Each model is a 2× multiplier with an output delayed by 1s.
//
//              ┌──────────────┐      ┌──────────────┐
//              │              │      │              │
// Input ●─────►│ multiplier 1 ├─────►│ multiplier 2 ├─────► Output
//              │              │      │              │
//              └──────────────┘      └──────────────┘
use nexosim::model::{Model, Output};
use nexosim::simulation::{Mailbox, SimInit};
use nexosim::time::{MonotonicTime, Scheduler};
use std::time::Duration;

// A model that doubles its input and forwards it with a 1s delay.
#[derive(Default)]
pub struct DelayedMultiplier {
    pub output: Output<f64>,
}
impl DelayedMultiplier {
    pub fn input(&mut self, value: f64, scheduler: &Scheduler<Self>) {
        scheduler
            .schedule_event(Duration::from_secs(1), Self::send, 2.0 * value)
            .unwrap();
    }
    async fn send(&mut self, value: f64) {
        self.output.send(value).await;
    }
}
impl Model for DelayedMultiplier {}

// Instantiate models and their mailboxes.
let mut multiplier1 = DelayedMultiplier::default();
let mut multiplier2 = DelayedMultiplier::default();
let multiplier1_mbox = Mailbox::new();
let multiplier2_mbox = Mailbox::new();

// Connect the output of `multiplier1` to the input of `multiplier2`.
multiplier1
    .output
    .connect(DelayedMultiplier::input, &multiplier2_mbox);

// Keep handles to the main input and output.
let mut output_slot = multiplier2.output.connect_slot().0;
let input_address = multiplier1_mbox.address();

// Instantiate the simulator
let t0 = MonotonicTime::EPOCH; // arbitrary start time
let mut simu = SimInit::new()
    .add_model(multiplier1, multiplier1_mbox)
    .add_model(multiplier2, multiplier2_mbox)
    .init(t0);

// Send a value to the first multiplier.
simu.send_event(DelayedMultiplier::input, 3.5, &input_address);

// Advance time to the next event.
simu.step();
assert_eq!(simu.time(), t0 + Duration::from_secs(1));
assert_eq!(output_slot.take(), None);

// Advance time to the next event.
simu.step();
assert_eq!(simu.time(), t0 + Duration::from_secs(2));
assert_eq!(output_slot.take(), Some(14.0));
```

# Implementation notes

Under the hood, NeXosim is based on an asynchronous implementation of the
[actor model][actor_model], where each simulation model is an actor. The
messages actually exchanged between models are `async` closures which capture
the event's or request's value and take the model as `&mut self` argument. The
mailbox associated to a model and to which closures are forwarded is the
receiver of an async, bounded MPSC channel.

Computations proceed at discrete times. When executed, models can request the
scheduler to send an event (or rather, a closure capturing such event) at a
certain simulation time. Whenever computations for the current time complete,
the scheduler selects the nearest future time at which one or several events are
scheduled (*next event increment*), thus triggering another set of computations.

This computational process makes it difficult to use general-purposes
asynchronous runtimes such as [Tokio][tokio], because the end of a set of
computations is technically a deadlock: the computation completes when all model
have nothing left to do and are blocked on an empty mailbox. Also, instead of
managing a conventional reactor, the runtime manages a priority queue containing
the posted events. For these reasons, NeXosim relies on a fully custom
runtime.

Even though the runtime was largely influenced by Tokio, it features additional
optimizations that make its faster than any other multi-threaded Rust executor
on the typically message-passing-heavy workloads seen in discrete-event
simulation (see [benchmark]). NeXosim also improves over the state of the
art with a very fast custom MPSC channel, which performance has been
demonstrated through [Tachyonix][tachyonix], a general-purpose offshoot of this
channel.

[actor_model]: https://en.wikipedia.org/wiki/Actor_model

[tokio]: https://github.com/tokio-rs/tokio

[tachyonix]: https://github.com/asynchronics/tachyonix

[benchmark]: https://github.com/asynchronics/tachyobench


## License

This software is licensed under the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.


## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
