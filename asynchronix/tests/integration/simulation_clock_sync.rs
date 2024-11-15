//! Loss of synchronization during simulation step execution.

use std::thread;
use std::time::Duration;

use asynchronix::model::Model;
use asynchronix::simulation::{ExecutionError, Mailbox, SimInit};
use asynchronix::time::{AutoSystemClock, MonotonicTime};

const MT_NUM_THREADS: usize = 4;

#[derive(Default)]
struct TestModel {}
impl TestModel {
    fn block_for(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}
impl Model for TestModel {}

// Schedule `TestModel::block_for` at the required ticks, blocking each time for
// the specified period, and then run the simulation with the specified
// synchronization tolerance.
//
// Returns the last simulation tick at completion or when the error occurred, and
// the result of `Simulation::step_until`.
fn clock_sync(
    num_threads: usize,
    block_time_ms: u64,
    clock_tolerance_ms: u64,
    ticks_ms: &[u64],
) -> (Duration, Result<(), ExecutionError>) {
    let block_time = Duration::from_millis(block_time_ms);
    let clock_tolerance = Duration::from_millis(clock_tolerance_ms);

    let model = TestModel::default();
    let clock = AutoSystemClock::new();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let (mut simu, scheduler) = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "test")
        .set_clock(clock)
        .set_clock_tolerance(clock_tolerance)
        .init(t0)
        .unwrap();

    let mut delta = Duration::ZERO;
    for tick_ms in ticks_ms {
        let tick = Duration::from_millis(*tick_ms);
        if tick > delta {
            delta = tick;
        }
        scheduler
            .schedule_event(tick, TestModel::block_for, block_time, &addr)
            .unwrap();
    }

    let res = simu.step_until(delta);
    let last_tick = simu.time().duration_since(t0);

    (last_tick, res)
}

fn clock_sync_zero_tolerance(num_threads: usize) {
    // The fourth tick should fail for being ~50ms too late.
    const BLOCKING_MS: u64 = 100;
    const CLOCK_TOLERANCE_MS: u64 = 0;
    const TICKS_MS: &[u64] = &[100, 250, 400, 450, 650];

    let (last_tick, res) = clock_sync(num_threads, BLOCKING_MS, CLOCK_TOLERANCE_MS, TICKS_MS);

    if let Err(ExecutionError::OutOfSync(_lag)) = res {
        assert_eq!(last_tick, Duration::from_millis(TICKS_MS[3]));
    } else {
        panic!("loss of synchronization not observed");
    }
}

fn clock_sync_with_tolerance(num_threads: usize) {
    // The third tick is ~50ms too late but should pass thanks to the tolerance.
    // The fifth tick should fail for being ~150ms too late, which is beyond the
    // 100ms tolerance.
    const BLOCKING_MS: u64 = 200;
    const CLOCK_TOLERANCE_MS: u64 = 100;
    const TICKS_MS: &[u64] = &[100, 350, 500, 800, 850, 1250];

    let (last_tick, res) = clock_sync(num_threads, BLOCKING_MS, CLOCK_TOLERANCE_MS, TICKS_MS);

    if let Err(ExecutionError::OutOfSync(lag)) = res {
        assert_eq!(last_tick, Duration::from_millis(TICKS_MS[4]));
        assert!(lag > Duration::from_millis(CLOCK_TOLERANCE_MS));
    } else {
        panic!("loss of synchronization not observed");
    }
}

#[test]
fn clock_sync_zero_tolerance_st() {
    clock_sync_zero_tolerance(1);
}

#[test]
fn clock_sync_zero_tolerance_mt() {
    clock_sync_zero_tolerance(MT_NUM_THREADS);
}

#[test]
fn clock_sync_with_tolerance_st() {
    clock_sync_with_tolerance(1);
}

#[test]
fn clock_sync_with_tolerance_mt() {
    clock_sync_with_tolerance(MT_NUM_THREADS);
}
