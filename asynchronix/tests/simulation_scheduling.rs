//! Event scheduling from a `Simulation` instance.

use std::time::Duration;

use asynchronix::model::{Model, Output};
use asynchronix::simulation::{Address, EventStream, Mailbox, SimInit, Simulation};
use asynchronix::time::MonotonicTime;

// Simple input-to-output pass-through model.
struct PassThroughModel<T: Clone + Send + 'static> {
    pub output: Output<T>,
}
impl<T: Clone + Send + 'static> PassThroughModel<T> {
    pub fn new() -> Self {
        Self {
            output: Output::default(),
        }
    }
    pub async fn input(&mut self, arg: T) {
        self.output.send(arg).await;
    }
}
impl<T: Clone + Send + 'static> Model for PassThroughModel<T> {}

/// A simple bench containing a single pass-through model (input forwarded to
/// output).
fn simple_bench<T: Clone + Send + 'static>() -> (
    Simulation,
    MonotonicTime,
    Address<PassThroughModel<T>>,
    EventStream<T>,
) {
    // Bench assembly.
    let mut model = PassThroughModel::new();
    let mbox = Mailbox::new();

    let out_stream = model.output.connect_stream().0;
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;

    let simu = SimInit::new().add_model(model, mbox).init(t0);

    (simu, t0, addr, out_stream)
}

#[test]
fn simulation_schedule_events() {
    let (mut simu, t0, addr, mut output) = simple_bench();

    // Queue 2 events at t0+3s and t0+2s, in reverse order.
    simu.schedule_event_in(Duration::from_secs(3), PassThroughModel::input, (), &addr)
        .unwrap();
    simu.schedule_event_at(
        t0 + Duration::from_secs(2),
        PassThroughModel::input,
        (),
        &addr,
    )
    .unwrap();

    // Move to the 1st event at t0+2s.
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_some());

    // Schedule another event in 4s (at t0+6s).
    simu.schedule_event_in(Duration::from_secs(4), PassThroughModel::input, (), &addr)
        .unwrap();

    // Move to the 2nd event at t0+3s.
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(3));
    assert!(output.next().is_some());

    // Move to the 3rd event at t0+6s.
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(6));
    assert!(output.next().is_some());
    assert!(output.next().is_none());
}

#[test]
fn simulation_schedule_keyed_events() {
    let (mut simu, t0, addr, mut output) = simple_bench();

    let event_t1 = simu
        .schedule_keyed_event_at(
            t0 + Duration::from_secs(1),
            PassThroughModel::input,
            1,
            &addr,
        )
        .unwrap();

    let event_t2_1 = simu
        .schedule_keyed_event_in(Duration::from_secs(2), PassThroughModel::input, 21, &addr)
        .unwrap();

    simu.schedule_event_in(Duration::from_secs(2), PassThroughModel::input, 22, &addr)
        .unwrap();

    // Move to the 1st event at t0+1.
    simu.step();

    // Try to cancel the 1st event after it has already taken place and check
    // that the cancellation had no effect.
    event_t1.cancel();
    assert_eq!(simu.time(), t0 + Duration::from_secs(1));
    assert_eq!(output.next(), Some(1));

    // Cancel the second event (t0+2) before it is meant to takes place and
    // check that we move directly to the 3rd event.
    event_t2_1.cancel();
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert_eq!(output.next(), Some(22));
    assert!(output.next().is_none());
}

#[test]
fn simulation_schedule_periodic_events() {
    let (mut simu, t0, addr, mut output) = simple_bench();

    // Queue 2 periodic events at t0 + 3s + k*2s.
    simu.schedule_periodic_event_in(
        Duration::from_secs(3),
        Duration::from_secs(2),
        PassThroughModel::input,
        1,
        &addr,
    )
    .unwrap();
    simu.schedule_periodic_event_at(
        t0 + Duration::from_secs(3),
        Duration::from_secs(2),
        PassThroughModel::input,
        2,
        &addr,
    )
    .unwrap();

    // Move to the next events at t0 + 3s + k*2s.
    for k in 0..10 {
        simu.step();
        assert_eq!(
            simu.time(),
            t0 + Duration::from_secs(3) + k * Duration::from_secs(2)
        );
        assert_eq!(output.next(), Some(1));
        assert_eq!(output.next(), Some(2));
        assert!(output.next().is_none());
    }
}

#[test]
fn simulation_schedule_periodic_keyed_events() {
    let (mut simu, t0, addr, mut output) = simple_bench();

    // Queue 2 periodic events at t0 + 3s + k*2s.
    simu.schedule_periodic_event_in(
        Duration::from_secs(3),
        Duration::from_secs(2),
        PassThroughModel::input,
        1,
        &addr,
    )
    .unwrap();
    let event2_key = simu
        .schedule_periodic_keyed_event_at(
            t0 + Duration::from_secs(3),
            Duration::from_secs(2),
            PassThroughModel::input,
            2,
            &addr,
        )
        .unwrap();

    // Move to the next event at t0+3s.
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(3));
    assert_eq!(output.next(), Some(1));
    assert_eq!(output.next(), Some(2));
    assert!(output.next().is_none());

    // Cancel the second event.
    event2_key.cancel();

    // Move to the next events at t0 + 3s + k*2s.
    for k in 1..10 {
        simu.step();
        assert_eq!(
            simu.time(),
            t0 + Duration::from_secs(3) + k * Duration::from_secs(2)
        );
        assert_eq!(output.next(), Some(1));
        assert!(output.next().is_none());
    }
}
