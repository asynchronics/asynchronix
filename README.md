# Asynchronix

A high-performance asynchronous computation framework for system simulation.

## What is this?

> **Warning**: this page is at the moment mostly addressed at interested
> contributors, but resources for users will be added soon. 

In a nutshell, Asynchronix is an effort to develop a framework for
discrete-event system simulation, with a particular focus on cyberphysical
systems. In this context, a system might be something as large as a spacecraft,
or as small as a IoT device.

Asynchronix draws from experience in the space industry but differs from
existing tools in a number of respects, including:

1) *open-source license*: it is distributed under the very permissive MIT and
   Apache 2 licenses, with the intent to foster an ecosystem where models can be
   easily exchanged without reliance on proprietary APIs,
2) *developer-friendly technology*: Rust's support for algebraic types and its
   powerful type system make it ideal for the "cyber" part in cyberphysical,
   i.e. for modelling digital devices with state machines,
3) *very fast*: by leveraging Rust's excellent support for multithreading and
   async programming, simulation models can run efficiently in parallel with all
   required synchronization being transparently handled by the simulator.


## General design

Asynchronix is an async compute framework for time-based discrete event
simulation.

From the perspective of simulation model implementers and users, it closely
resembles a flow-based programming framework: a model is essentially an isolated
entity with a fixed set of typed inputs and outputs, communicating with other
models and with the scheduler through message passing. Unlike in conventional
flow-based programming, however, request-response patterns are also possible.

Under the hood, Asynchronix' implementation is based on async Rust and the actor
model. All inputs are forwarded to a single "mailbox" (an async channel),
preserving the relative order of arrival of input messages.

Computations proceed at discrete times. When executed, models can post events
for the future, i.e. request the delayed activation of an input. Whenever the
computation at a given time completes, the scheduler selects the nearest future
time at which one or several events are scheduled, thus triggering another set
of computations.

This computational process makes it difficult to use general-purposes runtimes
such as Tokio, because the end of a set of computations is technically a
deadlock: the computation completes when all model have nothing left to do and
are blocked on an empty mailbox. Also, instead of managing a conventional
reactor, the runtime manages a priority queue containing the posted events. For
these reasons, it was necessary for Asynchronix to develop a fully custom
runtime.

Another crucial aspect of async compute is message-passing efficiency:
oftentimes the processing of an input is a simple action, making inter-thread
message-passing the bottleneck. This in turns calls for a very efficient
channel implementation, heavily optimized for the case of starved receivers
since models are most of the time waiting for an input to become available.


## Current state

The simulator is rapidly approaching MVP completion and has achieved 2 major
milestones:

* completion of an extremely fast asynchronous multi-threaded channel,
  demonstrated in the [Tachyonix][tachyonix] project; this channel is the
  backbone of the actor model,
* completion of a custom `async` executor optimized for message-passing and
  deadlock detection, which has demonstrated even better performance than Tokio
  for message-passing; this executor is already in the main branch and can be
  tested against other executors using the Tachyonix [benchmark].

Before it becomes usable, however, further work is required to implement the
priority queue, implement model inputs and outputs and adapt the channel.  

[tachyonix]: https://github.com/asynchronics/tachyonix

[benchmark]: https://github.com/asynchronics/tachyonix/tree/main/bench


## License

This software is licensed under the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.


## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.