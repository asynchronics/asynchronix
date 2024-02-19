//! Event scheduling within `Model` input methods.

use std::time::Duration;

use asynchronix::model::{Model, Output};
use asynchronix::simulation::{EventQueue, Mailbox, SimInit};
use asynchronix::time::{EventKey, MonotonicTime, Scheduler};

#[test]
fn model_schedule_event() {
    #[derive(Default)]
    struct TestModel {
        output: Output<()>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), scheduler: &Scheduler<Self>) {
            scheduler
                .schedule_event(scheduler.time() + Duration::from_secs(2), Self::action, ())
                .unwrap();
        }
        async fn action(&mut self) {
            self.output.send(()).await;
        }
    }
    impl Model for TestModel {}

    let mut model = TestModel::default();
    let mbox = Mailbox::new();

    let mut output = EventQueue::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox).init(t0);

    simu.send_event(TestModel::trigger, (), addr);
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_some());
    simu.step();
    assert!(output.next().is_none());
}

#[test]
fn model_cancel_future_keyed_event() {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
        key: Option<EventKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), scheduler: &Scheduler<Self>) {
            scheduler
                .schedule_event(scheduler.time() + Duration::from_secs(1), Self::action1, ())
                .unwrap();
            self.key = scheduler
                .schedule_keyed_event(scheduler.time() + Duration::from_secs(2), Self::action2, ())
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

    let mut output = EventQueue::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox).init(t0);

    simu.send_event(TestModel::trigger, (), addr);
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(1));
    assert_eq!(output.next(), Some(1));
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(1));
    assert!(output.next().is_none());
}

#[test]
fn model_cancel_same_time_keyed_event() {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
        key: Option<EventKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), scheduler: &Scheduler<Self>) {
            scheduler
                .schedule_event(scheduler.time() + Duration::from_secs(2), Self::action1, ())
                .unwrap();
            self.key = scheduler
                .schedule_keyed_event(scheduler.time() + Duration::from_secs(2), Self::action2, ())
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

    let mut output = EventQueue::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox).init(t0);

    simu.send_event(TestModel::trigger, (), addr);
    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert_eq!(output.next(), Some(1));
    assert!(output.next().is_none());
    simu.step();
    assert!(output.next().is_none());
}

#[test]
fn model_schedule_periodic_event() {
    #[derive(Default)]
    struct TestModel {
        output: Output<i32>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), scheduler: &Scheduler<Self>) {
            scheduler
                .schedule_periodic_event(
                    scheduler.time() + Duration::from_secs(2),
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

    let mut output = EventQueue::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox).init(t0);

    simu.send_event(TestModel::trigger, (), addr);

    // Move to the next events at t0 + 2s + k*3s.
    for k in 0..10 {
        simu.step();
        assert_eq!(
            simu.time(),
            t0 + Duration::from_secs(2) + k * Duration::from_secs(3)
        );
        assert_eq!(output.next(), Some(42));
        assert!(output.next().is_none());
    }
}

#[test]
fn model_cancel_periodic_event() {
    #[derive(Default)]
    struct TestModel {
        output: Output<()>,
        key: Option<EventKey>,
    }
    impl TestModel {
        fn trigger(&mut self, _: (), scheduler: &Scheduler<Self>) {
            self.key = scheduler
                .schedule_keyed_periodic_event(
                    scheduler.time() + Duration::from_secs(2),
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

    let mut output = EventQueue::new();
    model.output.connect_sink(&output);
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox).init(t0);

    simu.send_event(TestModel::trigger, (), addr);

    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_some());
    assert!(output.next().is_none());

    simu.step();
    assert_eq!(simu.time(), t0 + Duration::from_secs(2));
    assert!(output.next().is_none());
}
