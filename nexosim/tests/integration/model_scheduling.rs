//! Event scheduling within `Model` input methods.

use std::time::Duration;

use nexosim::model::{Context, Model};
use nexosim::ports::{EventBuffer, Output};
use nexosim::simulation::{ActionKey, Mailbox, SimInit};
use nexosim::time::MonotonicTime;

const MT_NUM_THREADS: usize = 4;

fn model_schedule_event(num_threads: usize) {
    #[derive(Default)]
    struct TestModel {
        output: Output<()>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), cx: &mut Context<Self>) {
            cx.schedule_event(Duration::from_secs(2), Self::action, ())
                .unwrap();
        }
        async fn action(&mut self) {
            self.output.send(()).await;
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventBuffer::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "")
        .init(t0)
        .unwrap()
        .0;

    simu.process_event(TestModel::trigger, (), addr).unwrap();
    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_some());
    simu.step().unwrap();
    assert!(output.next().is_none());
}

fn model_cancel_future_keyed_event(num_threads: usize) {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
        key: Option<ActionKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), cx: &mut Context<Self>) {
            cx.schedule_event(Duration::from_secs(1), Self::action1, ())
                .unwrap();
            self.key = cx
                .schedule_keyed_event(Duration::from_secs(2), Self::action2, ())
                .ok();
        }
        async fn action1(&mut self) {
            self.output.send(1).await;
            // Cancel the call to `action2`.
            self.key.take().unwrap().cancel();
        }
        async fn action2(&mut self) {
            self.output.send(2).await;
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventBuffer::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "")
        .init(t0)
        .unwrap()
        .0;

    simu.process_event(TestModel::trigger, (), addr).unwrap();
    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(1));
    assert_eq!(output.next(), Some(1));
    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(1));
    assert!(output.next().is_none());
}

fn model_cancel_same_time_keyed_event(num_threads: usize) {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
        key: Option<ActionKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), cx: &mut Context<Self>) {
            cx.schedule_event(Duration::from_secs(2), Self::action1, ())
                .unwrap();
            self.key = cx
                .schedule_keyed_event(Duration::from_secs(2), Self::action2, ())
                .ok();
        }
        async fn action1(&mut self) {
            self.output.send(1).await;
            // Cancel the call to `action2`.
            self.key.take().unwrap().cancel();
        }
        async fn action2(&mut self) {
            self.output.send(2).await;
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventBuffer::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "")
        .init(t0)
        .unwrap()
        .0;

    simu.process_event(TestModel::trigger, (), addr).unwrap();
    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert_eq!(output.next(), Some(1));
    assert!(output.next().is_none());
    simu.step().unwrap();
    assert!(output.next().is_none());
}

fn model_schedule_periodic_event(num_threads: usize) {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), cx: &mut Context<Self>) {
            cx.schedule_periodic_event(
                Duration::from_secs(2),
                Duration::from_secs(3),
                Self::action,
                42,
            )
            .unwrap();
        }
        async fn action(&mut self, payload: i32) {
            self.output.send(payload).await;
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventBuffer::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "")
        .init(t0)
        .unwrap()
        .0;

    simu.process_event(TestModel::trigger, (), addr).unwrap();

    // Move to the next events at t0 + 2s + k*3s.
    for k in 0..10 {
        simu.step().unwrap();
        assert_eq!(
            simu.time(),
            t0 + Duration::from_secs(2) + k * Duration::from_secs(3)
        );
        assert_eq!(output.next(), Some(42));
        assert!(output.next().is_none());
    }
}

fn model_cancel_periodic_event(num_threads: usize) {
    #[derive(Default)]
    struct TestModel {
        output: Output<()>,
        key: Option<ActionKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), cx: &mut Context<Self>) {
            self.key = cx
                .schedule_keyed_periodic_event(
                    Duration::from_secs(2),
                    Duration::from_secs(3),
                    Self::action,
                    (),
                )
                .ok();
        }
        async fn action(&mut self) {
            self.output.send(()).await;
            // Cancel the next events.
            self.key.take().unwrap().cancel();
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventBuffer::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "")
        .init(t0)
        .unwrap()
        .0;

    simu.process_event(TestModel::trigger, (), addr).unwrap();

    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_some());
    assert!(output.next().is_none());

    simu.step().unwrap();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_none());
}

#[test]
fn model_schedule_event_st() {
    model_schedule_event(1);
}

#[test]
fn model_schedule_event_mt() {
    model_schedule_event(MT_NUM_THREADS);
}

#[test]
fn model_cancel_future_keyed_event_st() {
    model_cancel_future_keyed_event(1);
}

#[test]
fn model_cancel_future_keyed_event_mt() {
    model_cancel_future_keyed_event(MT_NUM_THREADS);
}

#[test]
fn model_cancel_same_time_keyed_event_st() {
    model_cancel_same_time_keyed_event(1);
}

#[test]
fn model_cancel_same_time_keyed_event_mt() {
    model_cancel_same_time_keyed_event(MT_NUM_THREADS);
}

#[test]
fn model_schedule_periodic_event_st() {
    model_schedule_periodic_event(1);
}

#[test]
fn model_schedule_periodic_event_mt() {
    model_schedule_periodic_event(MT_NUM_THREADS);
}

#[test]
fn model_cancel_periodic_event_st() {
    model_cancel_periodic_event(1);
}

#[test]
fn model_cancel_periodic_event_mt() {
    model_cancel_periodic_event(MT_NUM_THREADS);
}
