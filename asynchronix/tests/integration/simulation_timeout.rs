//! Timeout during simulation step execution.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use asynchronix::model::Model;
use asynchronix::ports::Output;
use asynchronix::simulation::{ExecutionError, Mailbox, SimInit};
use asynchronix::time::MonotonicTime;

const MT_NUM_THREADS: usize = 4;

struct TestModel {
    output: Output<()>,
    // A liveliness flag that is cleared when the model is dropped.
    is_alive: Arc<AtomicBool>,
}
impl TestModel {
    fn new() -> (Self, Arc<AtomicBool>) {
        let is_alive = Arc::new(AtomicBool::new(true));

        (
            Self {
                output: Output::default(),
                is_alive: is_alive.clone(),
            },
            is_alive,
        )
    }

    async fn input(&mut self) {
        self.output.send(()).await;
    }
}
impl Drop for TestModel {
    fn drop(&mut self) {
        self.is_alive.store(false, Ordering::Relaxed);
    }
}
impl Model for TestModel {}

fn timeout_untriggered(num_threads: usize) {
    let (model, _model_is_alive) = TestModel::new();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "test")
        .set_timeout(Duration::from_secs(1))
        .init(t0)
        .unwrap()
        .0;

    assert!(simu.process_event(TestModel::input, (), addr).is_ok());
}

fn timeout_triggered(num_threads: usize) {
    let (mut model, model_is_alive) = TestModel::new();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    // Make a loopback connection.
    model.output.connect(TestModel::input, addr.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, "test")
        .set_timeout(Duration::from_secs(1))
        .init(t0)
        .unwrap()
        .0;

    assert!(matches!(
        simu.process_event(TestModel::input, (), addr),
        Err(ExecutionError::Timeout)
    ));

    // Make sure the request to stop the simulation has succeeded.
    thread::sleep(Duration::from_millis(10));
    assert!(!model_is_alive.load(Ordering::Relaxed));
}

#[test]
fn timeout_untriggered_st() {
    timeout_untriggered(1);
}

#[test]
fn timeout_untriggered_mt() {
    timeout_untriggered(MT_NUM_THREADS);
}

#[test]
fn timeout_triggered_st() {
    timeout_triggered(1);
}

#[test]
fn timeout_triggered_mt() {
    timeout_triggered(MT_NUM_THREADS);
}
