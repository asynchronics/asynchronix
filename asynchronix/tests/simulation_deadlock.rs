//! Deadlock-detection for model loops.

use asynchronix::model::Model;
use asynchronix::ports::{Output, Requestor};
use asynchronix::simulation::{ExecutionError, Mailbox, SimInit};
use asynchronix::time::MonotonicTime;

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

/// Overflows a mailbox by sending 2 messages in loopback for each incoming
/// message.
#[test]
fn deadlock_on_mailbox_overflow() {
    let mut model = TestModel::default();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    // Make two self-connections so that each outgoing message generates two
    // incoming messages.
    model
        .output
        .connect(TestModel::activate_output, addr.clone());
    model
        .output
        .connect(TestModel::activate_output, addr.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox, "").init(t0).unwrap();

    assert!(matches!(
        simu.process_event(TestModel::activate_output, (), addr),
        Err(ExecutionError::Deadlock(_))
    ));
}

/// Generates a deadlock with a query loopback.
#[test]
fn deadlock_on_query_loopback() {
    let mut model = TestModel::default();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    model
        .requestor
        .connect(TestModel::activate_requestor, addr.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new().add_model(model, mbox, "").init(t0).unwrap();

    assert!(matches!(
        simu.process_event(TestModel::activate_requestor, (), addr),
        Err(ExecutionError::Deadlock(_))
    ));
}

/// Generates a deadlock with a query loopback involving several models.
#[test]
fn deadlock_on_transitive_query_loopback() {
    let mut model1 = TestModel::default();
    let mut model2 = TestModel::default();
    let mbox1 = Mailbox::new();
    let mbox2 = Mailbox::new();
    let addr1 = mbox1.address();
    let addr2 = mbox2.address();

    model1
        .requestor
        .connect(TestModel::activate_requestor, addr2.clone());

    model2
        .requestor
        .connect(TestModel::activate_requestor, addr1.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::new()
        .add_model(model1, mbox1, "")
        .add_model(model2, mbox2, "")
        .init(t0)
        .unwrap();

    assert!(matches!(
        simu.process_event(TestModel::activate_requestor, (), addr1),
        Err(ExecutionError::Deadlock(_))
    ));
}
