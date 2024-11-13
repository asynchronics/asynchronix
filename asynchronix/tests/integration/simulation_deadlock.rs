//! Deadlock-detection for model loops.

use asynchronix::model::Model;
use asynchronix::ports::{Output, Requestor};
use asynchronix::simulation::{DeadlockInfo, ExecutionError, Mailbox, SimInit};
use asynchronix::time::MonotonicTime;

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

/// Overflows a mailbox by sending 2 messages in loopback for each incoming
/// message.
fn deadlock_on_mailbox_overflow(num_threads: usize) {
    const MODEL_NAME: &str = "testmodel";
    const MAILBOX_SIZE: usize = 5;

    let mut model = TestModel::default();
    let mbox = Mailbox::with_capacity(MAILBOX_SIZE);
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
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, MODEL_NAME)
        .init(t0)
        .unwrap();

    match simu.process_event(TestModel::activate_output, (), addr) {
        Err(ExecutionError::Deadlock(deadlock_info)) => {
            // We expect only 1 deadlocked model.
            assert_eq!(deadlock_info.len(), 1);
            // We expect the mailbox to be full.
            assert_eq!(
                deadlock_info[0],
                DeadlockInfo {
                    model: MODEL_NAME.into(),
                    mailbox_size: MAILBOX_SIZE
                }
            )
        }
        _ => panic!("deadlock not detected"),
    }
}

/// Generates a deadlock with a query loopback.
fn deadlock_on_query_loopback(num_threads: usize) {
    const MODEL_NAME: &str = "testmodel";

    let mut model = TestModel::default();
    let mbox = Mailbox::new();
    let addr = mbox.address();

    model
        .requestor
        .connect(TestModel::activate_requestor, addr.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model, mbox, MODEL_NAME)
        .init(t0)
        .unwrap();

    match simu.process_query(TestModel::activate_requestor, (), addr) {
        Err(ExecutionError::Deadlock(deadlock_info)) => {
            // We expect only 1 deadlocked model.
            assert_eq!(deadlock_info.len(), 1);
            // We expect the mailbox to have a single query.
            assert_eq!(
                deadlock_info[0],
                DeadlockInfo {
                    model: MODEL_NAME.into(),
                    mailbox_size: 1,
                }
            );
        }
        _ => panic!("deadlock not detected"),
    }
}

/// Generates a deadlock with a query loopback involving several models.
fn deadlock_on_transitive_query_loopback(num_threads: usize) {
    const MODEL1_NAME: &str = "testmodel1";
    const MODEL2_NAME: &str = "testmodel2";

    let mut model1 = TestModel::default();
    let mut model2 = TestModel::default();
    let mbox1 = Mailbox::new();
    let mbox2 = Mailbox::new();
    let addr1 = mbox1.address();
    let addr2 = mbox2.address();

    model1
        .requestor
        .connect(TestModel::activate_requestor, addr2);

    model2
        .requestor
        .connect(TestModel::activate_requestor, addr1.clone());

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model1, mbox1, MODEL1_NAME)
        .add_model(model2, mbox2, MODEL2_NAME)
        .init(t0)
        .unwrap();

    match simu.process_query(TestModel::activate_requestor, (), addr1) {
        Err(ExecutionError::Deadlock(deadlock_info)) => {
            // We expect only 1 deadlocked model.
            assert_eq!(deadlock_info.len(), 1);
            // We expect the mailbox of this model to have a single query.
            assert_eq!(
                deadlock_info[0],
                DeadlockInfo {
                    model: MODEL1_NAME.into(),
                    mailbox_size: 1,
                }
            );
        }
        _ => panic!("deadlock not detected"),
    }
}

/// Generates deadlocks with query loopbacks on several models at the same time.
fn deadlock_on_multiple_query_loopback(num_threads: usize) {
    const MODEL0_NAME: &str = "testmodel0";
    const MODEL1_NAME: &str = "testmodel1";
    const MODEL2_NAME: &str = "testmodel2";

    let mut model0 = TestModel::default();
    let mut model1 = TestModel::default();
    let mut model2 = TestModel::default();
    let mbox0 = Mailbox::new();
    let mbox1 = Mailbox::new();
    let mbox2 = Mailbox::new();
    let addr0 = mbox0.address();
    let addr1 = mbox1.address();
    let addr2 = mbox2.address();

    model0
        .requestor
        .connect(TestModel::activate_requestor, addr1.clone());

    model0
        .requestor
        .connect(TestModel::activate_requestor, addr2.clone());

    model1
        .requestor
        .connect(TestModel::activate_requestor, addr1);

    model2
        .requestor
        .connect(TestModel::activate_requestor, addr2);

    let t0 = MonotonicTime::EPOCH;
    let mut simu = SimInit::with_num_threads(num_threads)
        .add_model(model0, mbox0, MODEL0_NAME)
        .add_model(model1, mbox1, MODEL1_NAME)
        .add_model(model2, mbox2, MODEL2_NAME)
        .init(t0)
        .unwrap();

    match simu.process_query(TestModel::activate_requestor, (), addr0) {
        Err(ExecutionError::Deadlock(deadlock_info)) => {
            // We expect 2 deadlocked models.
            assert_eq!(deadlock_info.len(), 2);
            // We expect the mailbox of each deadlocked model to have a single
            // query.
            assert_eq!(
                deadlock_info[0],
                DeadlockInfo {
                    model: MODEL1_NAME.into(),
                    mailbox_size: 1,
                }
            );
            assert_eq!(
                deadlock_info[1],
                DeadlockInfo {
                    model: MODEL2_NAME.into(),
                    mailbox_size: 1,
                }
            );
        }
        _ => panic!("deadlock not detected"),
    }
}

#[test]
fn deadlock_on_mailbox_overflow_st() {
    deadlock_on_mailbox_overflow(1);
}

#[test]
fn deadlock_on_mailbox_overflow_mt() {
    deadlock_on_mailbox_overflow(MT_NUM_THREADS);
}

#[test]
fn deadlock_on_query_loopback_st() {
    deadlock_on_query_loopback(1);
}

#[test]
fn deadlock_on_query_loopback_mt() {
    deadlock_on_query_loopback(MT_NUM_THREADS);
}

#[test]
fn deadlock_on_transitive_query_loopback_st() {
    deadlock_on_transitive_query_loopback(1);
}

#[test]
fn deadlock_on_transitive_query_loopback_mt() {
    deadlock_on_transitive_query_loopback(MT_NUM_THREADS);
}

#[test]
fn deadlock_on_multiple_query_loopback_st() {
    deadlock_on_multiple_query_loopback(1);
}

#[test]
fn deadlock_on_multiple_query_loopback_mt() {
    deadlock_on_multiple_query_loopback(MT_NUM_THREADS);
}
