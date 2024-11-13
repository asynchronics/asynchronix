// Integration tests follow the organization suggested by Matklad:
// https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html

mod model_scheduling;
#[cfg(not(miri))]
mod simulation_clock_sync;
mod simulation_deadlock;
mod simulation_panic;
mod simulation_scheduling;
#[cfg(not(miri))]
mod simulation_timeout;
