//! Model panic reporting.

use asynchronix::model::Model;
use asynchronix::ports::Output;
use asynchronix::simulation::{ExecutionError, Mailbox, SimInit};
use asynchronix::time::MonotonicTime;

const MT_NUM_THREADS: usize = 4;

#[derive(Default)]
struct TestModel {
    countdown_out: Output<usize>,
}
impl TestModel {
    async fn countdown_in(&mut self, count: usize) {
        if count == 0 {
            panic!("test message");
        }
        self.countdown_out.send(count - 1).await;
    }
}
impl Model for TestModel {}

/// Pass a counter around several models and decrement it each time, panicking
/// when it becomes zero.
fn model_panic(num_threads: usize) {
    const MODEL_COUNT: usize = 5;
    const INIT_COUNTDOWN: usize = 9;

    // Connect all models in a cycle graph.
    let mut model0 = TestModel::default();
    let mbox0 = Mailbox::new();
    let addr0 = mbox0.address();

    let mut siminit = SimInit::with_num_threads(num_threads);

    let mut addr = mbox0.address();
    for model_id in (1..MODEL_COUNT).rev() {
        let mut model = TestModel::default();
        let mbox = Mailbox::new();
        model.countdown_out.connect(TestModel::countdown_in, addr);
        addr = mbox.address();
        siminit = siminit.add_model(model, mbox, model_id.to_string());
    }

    model0.countdown_out.connect(TestModel::countdown_in, addr);
    siminit = siminit.add_model(model0, mbox0, 0.to_string());

    // Run the simulation.
    let t0 = MonotonicTime::EPOCH;
    let mut simu = siminit.init(t0).unwrap();

    match simu.process_event(TestModel::countdown_in, INIT_COUNTDOWN, addr0) {
        Err(ExecutionError::Panic { model, payload }) => {
            let msg = payload.downcast_ref::<&str>().unwrap();
            let panicking_model_id = INIT_COUNTDOWN % MODEL_COUNT;

            assert_eq!(model, panicking_model_id.to_string());
            assert_eq!(*msg, "test message");
        }
        _ => panic!("panic not detected"),
    }
}

#[test]
fn model_panic_st() {
    model_panic(1);
}

#[test]
fn model_panic_mt() {
    model_panic(MT_NUM_THREADS);
}
