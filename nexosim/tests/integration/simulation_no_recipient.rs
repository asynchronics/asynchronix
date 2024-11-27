//! Missing recipient detection.

use std::time::Duration;

use nexosim::model::Model;
use nexosim::ports::{EventSource, Output, QuerySource, Requestor};
use nexosim::simulation::{ExecutionError, Mailbox, SimInit};
use nexosim::time::MonotonicTime;

const MT_NUM_THREADS: usize = 4;

#[derive(Default)]
struct TestModel {
    output: Output<()>,
    requestor: Requestor<(), ()>,
}
impl TestModel {
    async fn activate_output(&mut self) {
        self.output.send(()).await;
    }
    async fn activate_requestor(&mut self) {
        let _ = self.requestor.send(()).await;
    }
}
impl Model for TestModel {}

/// Send an event from a model to a dead input.
fn no_input_from_model(num_threads: usize) {
    const MODEL_NAME: &str = "testmodel";

    let mut model = TestModel::default();
    let mbox = Mailbox::new();
    let addr = mbox.address();
    let bad_mbox = Mailbox::new();

    model.output.connect(TestModel::activate_output, &bad_mbox);

    drop(bad_mbox);

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, MODEL_NAME)
        .init(t0)
        .unwrap()
        .0;

    match simu.process_event(TestModel::activate_output, (), addr) {
        Err(ExecutionError::NoRecipient { model }) => {
            assert_eq!(model, Some(String::from(MODEL_NAME)));
        }
        _ => panic!("missing recipient not detected"),
    }
}

/// Send an event from a model to a dead replier.
fn no_replier_from_model(num_threads: usize) {
    const MODEL_NAME: &str = "testmodel";

    let mut model = TestModel::default();
    let mbox = Mailbox::new();
    let addr = mbox.address();
    let bad_mbox = Mailbox::new();

    model
        .requestor
        .connect(TestModel::activate_requestor, &bad_mbox);

    drop(bad_mbox);

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, MODEL_NAME)
        .init(t0)
        .unwrap()
        .0;

    match simu.process_event(TestModel::activate_requestor, (), addr) {
        Err(ExecutionError::NoRecipient { model }) => {
            assert_eq!(model, Some(String::from(MODEL_NAME)));
        }
        _ => panic!("missing recipient not detected"),
    }
}

/// Send an event from the scheduler to a dead input.
fn no_input_from_scheduler(num_threads: usize) {
    let bad_mbox = Mailbox::new();

    let mut src = EventSource::new();
    src.connect(TestModel::activate_output, &bad_mbox);
    let event = src.event(());

    drop(bad_mbox);

    let t0 = MonotonicTime::EPOCH;
    let (mut simu, scheduler) = SimInit::with_num_threads(num_threads).init(t0).unwrap();

    scheduler.schedule(Duration::from_secs(1), event).unwrap();

    match simu.step() {
        Err(ExecutionError::NoRecipient { model }) => {
            assert_eq!(model, None);
        }
        _ => panic!("missing recipient not detected"),
    }
}

/// Send a query from the scheduler to a dead input.
fn no_replier_from_scheduler(num_threads: usize) {
    let bad_mbox = Mailbox::new();

    let mut src = QuerySource::new();
    src.connect(TestModel::activate_requestor, &bad_mbox);
    let query = src.query(()).0;

    drop(bad_mbox);

    let t0 = MonotonicTime::EPOCH;
    let (mut simu, scheduler) = SimInit::with_num_threads(num_threads).init(t0).unwrap();

    scheduler.schedule(Duration::from_secs(1), query).unwrap();

    match simu.step() {
        Err(ExecutionError::NoRecipient { model }) => {
            assert_eq!(model, None);
        }
        _ => panic!("missing recipient not detected"),
    }
}

#[test]
fn no_input_from_model_st() {
    no_input_from_model(1);
}

#[test]
fn no_input_from_model_mt() {
    no_input_from_model(MT_NUM_THREADS);
}

#[test]
fn no_replier_from_model_st() {
    no_replier_from_model(1);
}

#[test]
fn no_replier_from_model_mt() {
    no_replier_from_model(MT_NUM_THREADS);
}

#[test]
fn no_input_from_scheduler_st() {
    no_input_from_scheduler(1);
}

#[test]
fn no_input_from_scheduler_mt() {
    no_input_from_scheduler(MT_NUM_THREADS);
}

#[test]
fn no_replier_from_scheduler_st() {
    no_replier_from_scheduler(1);
}

#[test]
fn no_replier_from_scheduler_mt() {
    no_replier_from_scheduler(MT_NUM_THREADS);
}
