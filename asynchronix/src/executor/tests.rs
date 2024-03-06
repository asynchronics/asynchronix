use futures_channel::{mpsc, oneshot};
use futures_util::StreamExt;

use super::*;

/// An object that runs an arbitrary closure when dropped.
struct RunOnDrop<F: FnOnce()> {
    drop_fn: Option<F>,
}
impl<F: FnOnce()> RunOnDrop<F> {
    /// Creates a new `RunOnDrop`.
    fn new(drop_fn: F) -> Self {
        Self {
            drop_fn: Some(drop_fn),
        }
    }
}
impl<F: FnOnce()> Drop for RunOnDrop<F> {
    fn drop(&mut self) {
        self.drop_fn.take().map(|f| f());
    }
}

#[test]
fn executor_deadlock() {
    const NUM_THREADS: usize = 3;

    let (_sender1, receiver1) = oneshot::channel::<()>();
    let (_sender2, receiver2) = oneshot::channel::<()>();

    let mut executor = Executor::new(NUM_THREADS);
    static LAUNCH_COUNT: AtomicUsize = AtomicUsize::new(0);
    static COMPLETION_COUNT: AtomicUsize = AtomicUsize::new(0);

    executor.spawn_and_forget(async move {
        LAUNCH_COUNT.fetch_add(1, Ordering::Relaxed);
        let _ = receiver2.await;
        COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
    });
    executor.spawn_and_forget(async move {
        LAUNCH_COUNT.fetch_add(1, Ordering::Relaxed);
        let _ = receiver1.await;
        COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
    });

    executor.run();
    // Check that the executor returns on deadlock, i.e. none of the task has
    // completed.
    assert_eq!(LAUNCH_COUNT.load(Ordering::Relaxed), 2);
    assert_eq!(COMPLETION_COUNT.load(Ordering::Relaxed), 0);
}

#[test]
fn executor_deadlock_st() {
    const NUM_THREADS: usize = 1;

    let (_sender1, receiver1) = oneshot::channel::<()>();
    let (_sender2, receiver2) = oneshot::channel::<()>();

    let mut executor = Executor::new(NUM_THREADS);
    static LAUNCH_COUNT: AtomicUsize = AtomicUsize::new(0);
    static COMPLETION_COUNT: AtomicUsize = AtomicUsize::new(0);

    executor.spawn_and_forget(async move {
        LAUNCH_COUNT.fetch_add(1, Ordering::Relaxed);
        let _ = receiver2.await;
        COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
    });
    executor.spawn_and_forget(async move {
        LAUNCH_COUNT.fetch_add(1, Ordering::Relaxed);
        let _ = receiver1.await;
        COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
    });

    executor.run();
    // Check that the executor returnes on deadlock, i.e. none of the task has
    // completed.
    assert_eq!(LAUNCH_COUNT.load(Ordering::Relaxed), 2);
    assert_eq!(COMPLETION_COUNT.load(Ordering::Relaxed), 0);
}

#[test]
fn executor_drop_cycle() {
    const NUM_THREADS: usize = 3;

    let (sender1, mut receiver1) = mpsc::channel(2);
    let (sender2, mut receiver2) = mpsc::channel(2);
    let (sender3, mut receiver3) = mpsc::channel(2);

    let mut executor = Executor::new(NUM_THREADS);
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    // Spawn 3 tasks that wake one another when dropped.
    executor.spawn_and_forget({
        let mut sender2 = sender2.clone();
        let mut sender3 = sender3.clone();

        async move {
            let _guard = RunOnDrop::new(move || {
                let _ = sender2.try_send(());
                let _ = sender3.try_send(());
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            });
            let _ = receiver1.next().await;
        }
    });
    executor.spawn_and_forget({
        let mut sender1 = sender1.clone();
        let mut sender3 = sender3.clone();

        async move {
            let _guard = RunOnDrop::new(move || {
                let _ = sender1.try_send(());
                let _ = sender3.try_send(());
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            });
            let _ = receiver2.next().await;
        }
    });
    executor.spawn_and_forget({
        let mut sender1 = sender1.clone();
        let mut sender2 = sender2.clone();

        async move {
            let _guard = RunOnDrop::new(move || {
                let _ = sender1.try_send(());
                let _ = sender2.try_send(());
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            });
            let _ = receiver3.next().await;
        }
    });

    executor.run();

    // Make sure that all tasks are eventually dropped even though each task
    // wakes the others when dropped.
    drop(executor);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 3);
}
