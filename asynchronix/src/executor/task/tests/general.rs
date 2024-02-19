use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;

use futures_channel::{mpsc, oneshot};
use futures_util::StreamExt;

use super::super::promise::Stage;
use super::*;

// Test prelude to simulates a single-slot scheduler queue.
macro_rules! test_prelude {
    () => {
        static QUEUE: Mutex<Vec<Runnable>> = Mutex::new(Vec::new());

        // Schedules one runnable task.
        //
        // Will panic if the slot was already occupied since there should exist
        // at most 1 runnable per task at any time.
        #[allow(dead_code)]
        fn schedule_runnable(runnable: Runnable, _tag: ()) {
            let mut queue = QUEUE.lock().unwrap();
            queue.push(runnable);
        }

        // Runs one runnable task and returns true if a task was scheduled,
        // otherwise returns false.
        #[allow(dead_code)]
        fn run_scheduled_runnable() -> bool {
            if let Some(runnable) = QUEUE.lock().unwrap().pop() {
                runnable.run();
                return true;
            }

            false
        }

        // Drops a runnable task and returns true if a task was scheduled, otherwise
        // returns false.
        #[allow(dead_code)]
        fn drop_runnable() -> bool {
            if let Some(_runnable) = QUEUE.lock().unwrap().pop() {
                return true;
            }

            false
        }
    };
}

// A friendly wrapper over a shared atomic boolean that uses only Relaxed
// ordering.
#[derive(Clone)]
struct Flag(Arc<AtomicBool>);
impl Flag {
    fn new(value: bool) -> Self {
        Self(Arc::new(AtomicBool::new(value)))
    }
    fn set(&self, value: bool) {
        self.0.store(value, Ordering::Relaxed);
    }
    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// A simple wrapper for the output of a future with a liveness flag.
struct MonitoredOutput<T> {
    is_alive: Flag,
    inner: T,
}
impl<T> Deref for MonitoredOutput<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}
impl<T> Drop for MonitoredOutput<T> {
    fn drop(&mut self) {
        self.is_alive.set(false);
    }
}

// A simple future wrapper with a liveness flag returning a `MonitoredOutput` on
// completion.
struct MonitoredFuture<F: Future> {
    future_is_alive: Flag,
    output_is_alive: Flag,
    inner: F,
}
impl<F: Future> MonitoredFuture<F> {
    // Returns the `MonitoredFuture`, a liveness flag for the future and a
    // liveness flag for the output.
    fn new(future: F) -> (Self, Flag, Flag) {
        let future_is_alive = Flag::new(true);
        let output_is_alive = Flag::new(false);
        let future_is_alive_remote = future_is_alive.clone();
        let output_is_alive_remote = output_is_alive.clone();

        (
            Self {
                future_is_alive,
                output_is_alive,
                inner: future,
            },
            future_is_alive_remote,
            output_is_alive_remote,
        )
    }
}
impl<F: Future> Future for MonitoredFuture<F> {
    type Output = MonitoredOutput<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = unsafe { self.as_mut().map_unchecked_mut(|s| &mut s.inner) };
        match inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(value) => {
                self.output_is_alive.set(true);
                let test_output = MonitoredOutput {
                    is_alive: self.output_is_alive.clone(),
                    inner: value,
                };
                Poll::Ready(test_output)
            }
        }
    }
}
impl<F: Future> Drop for MonitoredFuture<F> {
    fn drop(&mut self) {
        self.future_is_alive.set(false);
    }
}

#[test]
fn task_schedule() {
    test_prelude!();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move { 42 });
    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    assert_eq!(future_is_alive.get(), true);
    assert_eq!(output_is_alive.get(), false);

    // The task should complete immediately when ran.
    runnable.run();
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
    assert_eq!(promise.poll().map(|v| *v), Stage::Ready(42));
}

#[test]
fn task_schedule_mt() {
    test_prelude!();

    let (promise, runnable, _cancel_token) = spawn(async move { 42 }, schedule_runnable, ());

    let th = thread::spawn(move || runnable.run());
    loop {
        match promise.poll() {
            Stage::Pending => {}
            Stage::Cancelled => unreachable!(),
            Stage::Ready(v) => {
                assert_eq!(v, 42);
                break;
            }
        }
    }
    th.join().unwrap();
}

#[test]
fn task_schedule_and_forget() {
    test_prelude!();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async {});
    let (runnable, _cancel_token) = spawn_and_forget(future, schedule_runnable, ());
    assert_eq!(future_is_alive.get(), true);
    assert_eq!(output_is_alive.get(), false);

    // The task should complete immediately when ran.
    runnable.run();
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
}

#[test]
fn task_wake() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let result = receiver.await.unwrap();
        result
    });

    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();

    // The future should have been polled but should not have completed.
    assert_eq!(output_is_alive.get(), false);
    assert!(promise.poll().is_pending());

    // Wake the task.
    sender.send(42).unwrap();

    // The task should have been scheduled by the channel sender.
    assert_eq!(run_scheduled_runnable(), true);
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
    assert_eq!(promise.poll().map(|v| *v), Stage::Ready(42));
}

#[test]
fn task_wake_mt() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (promise, runnable, _cancel_token) = spawn(
        async move {
            let result = receiver.await.unwrap();
            result
        },
        schedule_runnable,
        (),
    );
    runnable.run();

    let th_sender = thread::spawn(move || sender.send(42).unwrap());
    let th_exec = thread::spawn(|| while !run_scheduled_runnable() {});

    loop {
        match promise.poll() {
            Stage::Pending => {}
            Stage::Cancelled => unreachable!(),
            Stage::Ready(v) => {
                assert_eq!(v, 42);
                break;
            }
        }
    }
    th_sender.join().unwrap();
    th_exec.join().unwrap();
}

#[test]
fn task_wake_and_forget() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let _ = receiver.await;
    });

    let (runnable, _cancel_token) = spawn_and_forget(future, schedule_runnable, ());
    runnable.run();

    // The future should have been polled but should not have completed.
    assert_eq!(output_is_alive.get(), false);

    // Wake the task.
    sender.send(42).unwrap();

    // The task should have been scheduled by the channel sender.
    assert_eq!(run_scheduled_runnable(), true);
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
}

#[test]
fn task_multiple_wake() {
    test_prelude!();

    let (mut sender, mut receiver) = mpsc::channel(3);

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let mut sum = 0;
        for _ in 0..5 {
            sum += receiver.next().await.unwrap();
        }
        sum
    });

    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();

    // The future should have been polled but should not have completed.
    assert!(promise.poll().is_pending());

    // Wake the task 3 times.
    sender.try_send(1).unwrap();
    sender.try_send(2).unwrap();
    sender.try_send(3).unwrap();

    // The task should have been scheduled by the channel sender.
    assert_eq!(run_scheduled_runnable(), true);
    assert!(promise.poll().is_pending());

    // The channel should be empty. Wake the task 2 more times.
    sender.try_send(4).unwrap();
    sender.try_send(5).unwrap();

    // The task should have been scheduled by the channel sender.
    assert_eq!(run_scheduled_runnable(), true);

    // The task should have completed.
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
    assert_eq!(promise.poll().map(|v| *v), Stage::Ready(15));
}

#[test]
fn task_multiple_wake_mt() {
    test_prelude!();

    let (mut sender1, mut receiver) = mpsc::channel(3);
    let mut sender2 = sender1.clone();
    let mut sender3 = sender1.clone();

    let (promise, runnable, _cancel_token) = spawn(
        async move {
            let mut sum = 0;
            for _ in 0..3 {
                sum += receiver.next().await.unwrap();
            }
            sum
        },
        schedule_runnable,
        (),
    );
    runnable.run();

    // Wake the task 3 times.
    let th_sender1 = thread::spawn(move || {
        sender1.try_send(1).unwrap();
        while run_scheduled_runnable() {}
    });
    let th_sender2 = thread::spawn(move || {
        sender2.try_send(2).unwrap();
        while run_scheduled_runnable() {}
    });
    let th_sender3 = thread::spawn(move || {
        sender3.try_send(3).unwrap();
        while run_scheduled_runnable() {}
    });

    loop {
        match promise.poll() {
            Stage::Pending => {}
            Stage::Cancelled => unreachable!(),
            Stage::Ready(v) => {
                assert_eq!(v, 6);
                break;
            }
        }
    }
    th_sender1.join().unwrap();
    th_sender2.join().unwrap();
    th_sender3.join().unwrap();
}

#[test]
fn task_cancel_scheduled() {
    test_prelude!();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async {});

    let (promise, runnable, cancel_token) = spawn(future, schedule_runnable, ());

    // Cancel the task while a `Runnable` exists (i.e. while the task is
    // considered scheduled).
    cancel_token.cancel();

    // The future should not be dropped while the `Runnable` exists, even if the
    // task is cancelled, but the task should be seen as cancelled.
    assert_eq!(future_is_alive.get(), true);
    assert!(promise.poll().is_cancelled());

    // An attempt to run the task should now drop the future without polling it.
    runnable.run();
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), false);
}

#[test]
fn task_cancel_unscheduled() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let _ = receiver.await;
    });

    let (promise, runnable, cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();
    assert_eq!(future_is_alive.get(), true);
    assert_eq!(output_is_alive.get(), false);

    // Cancel the task while no `Runnable` exists (the task is not scheduled as
    // it needs to be woken by the channel sender first).
    cancel_token.cancel();
    assert!(promise.poll().is_cancelled());
    assert!(sender.send(()).is_err());

    // The future should be dropped immediately upon cancellation without
    // completing.
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), false);
}

#[test]
fn task_cancel_completed() {
    test_prelude!();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move { 42 });

    let (promise, runnable, cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);

    // Cancel the already completed task.
    cancel_token.cancel();
    assert_eq!(output_is_alive.get(), true);
    assert_eq!(promise.poll().map(|v| *v), Stage::Ready(42));
}

#[test]
fn task_cancel_mt() {
    test_prelude!();

    let (runnable, cancel_token) = spawn_and_forget(async {}, schedule_runnable, ());

    let th_cancel = thread::spawn(move || cancel_token.cancel());
    runnable.run();

    th_cancel.join().unwrap();
}

#[test]
fn task_drop_promise_scheduled() {
    test_prelude!();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async {});

    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    // Drop the promise while a `Runnable` exists (i.e. while the task is
    // considered scheduled).
    drop(promise);

    // The task should complete immediately when ran.
    runnable.run();
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
}

#[test]
fn task_drop_promise_unscheduled() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let _ = receiver.await;
    });

    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();

    // Drop the promise while no `Runnable` exists (the task is not scheduled as
    // it needs to be woken by the channel sender first).
    drop(promise);

    // Wake the task.
    assert!(sender.send(()).is_ok());

    // The task should have been scheduled by the channel sender.
    assert_eq!(run_scheduled_runnable(), true);
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), true);
}

#[test]
fn task_drop_promise_mt() {
    test_prelude!();

    let (promise, runnable, _cancel_token) = spawn(async {}, schedule_runnable, ());

    let th_drop = thread::spawn(move || drop(promise));
    runnable.run();

    th_drop.join().unwrap()
}

#[test]
fn task_drop_runnable() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (future, future_is_alive, output_is_alive) = MonitoredFuture::new(async move {
        let _ = receiver.await;
    });

    let (promise, runnable, _cancel_token) = spawn(future, schedule_runnable, ());
    runnable.run();

    // Wake the task.
    assert!(sender.send(()).is_ok());

    // Drop the task scheduled by the channel sender.
    assert_eq!(drop_runnable(), true);
    assert_eq!(future_is_alive.get(), false);
    assert_eq!(output_is_alive.get(), false);
    assert!(promise.poll().is_cancelled());
}

#[test]
fn task_drop_runnable_mt() {
    test_prelude!();

    let (sender, receiver) = oneshot::channel();

    let (runnable, _cancel_token) = spawn_and_forget(
        async move {
            let _ = receiver.await;
        },
        schedule_runnable,
        (),
    );
    runnable.run();

    let th_sender = thread::spawn(move || sender.send(()).is_ok());
    drop_runnable();

    th_sender.join().unwrap();
}

#[test]
fn task_drop_cycle() {
    test_prelude!();

    let (sender1, mut receiver1) = mpsc::channel(2);
    let (sender2, mut receiver2) = mpsc::channel(2);
    let (sender3, mut receiver3) = mpsc::channel(2);

    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    // Spawn 3 tasks that wake one another when dropped.
    let (runnable1, cancel_token1) = spawn_and_forget(
        {
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
        },
        schedule_runnable,
        (),
    );
    runnable1.run();

    let (runnable2, cancel_token2) = spawn_and_forget(
        {
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
        },
        schedule_runnable,
        (),
    );
    runnable2.run();

    let (runnable3, cancel_token3) = spawn_and_forget(
        {
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
        },
        schedule_runnable,
        (),
    );
    runnable3.run();

    let th1 = thread::spawn(move || cancel_token1.cancel());
    let th2 = thread::spawn(move || cancel_token2.cancel());
    let th3 = thread::spawn(move || cancel_token3.cancel());

    th1.join().unwrap();
    th2.join().unwrap();
    th3.join().unwrap();

    while run_scheduled_runnable() {}

    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 3);
}
