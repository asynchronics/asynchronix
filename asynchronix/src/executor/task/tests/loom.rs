use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use ::loom::cell::UnsafeCell;
use ::loom::model::Builder;
use ::loom::sync::atomic::AtomicBool;
use ::loom::sync::atomic::AtomicUsize;
use ::loom::sync::atomic::Ordering::*;
use ::loom::sync::Arc;
use ::loom::{lazy_static, thread};

use super::promise::Stage;
use super::*;

// Test prelude to simulates a single-slot scheduler queue.
macro_rules! test_prelude {
    () => {
        // A single-slot scheduling queue.
        lazy_static! {
            static ref RUNNABLE_SLOT: RunnableSlot = RunnableSlot::new();
        }

        // Schedules one runnable task.
        //
        // Will panic if the slot was already occupied since there should exist
        // at most 1 runnable per task at any time.
        #[allow(dead_code)]
        fn schedule_task(runnable: Runnable, _tag: ()) {
            RUNNABLE_SLOT.set(runnable);
        }

        // Runs one runnable task and returns true if a task was indeed
        // scheduled, otherwise returns false.
        #[allow(dead_code)]
        fn try_poll_task() -> bool {
            if let Some(runnable) = RUNNABLE_SLOT.take() {
                runnable.run();
                return true;
            }

            false
        }

        // Cancel a scheduled task by dropping its runnable and returns true is
        // a task was indeed scheduled, otherwise returns false.
        #[allow(dead_code)]
        fn try_cancel_task() -> bool {
            if let Some(_runnable) = RUNNABLE_SLOT.take() {
                // Just drop the runnable to cancel the task.
                return true;
            }

            false
        }
    };
}

struct RunnableSlot {
    state: AtomicUsize,
    runnable: UnsafeCell<Option<Runnable>>,
}
impl RunnableSlot {
    const LOCKED: usize = 0b01;
    const POPULATED: usize = 0b10;

    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            runnable: UnsafeCell::new(None),
        }
    }

    fn take(&self) -> Option<Runnable> {
        self.state
            .fetch_update(Acquire, Relaxed, |s| {
                // Only lock if there is a runnable and it is not already locked.
                if s == Self::POPULATED {
                    Some(Self::LOCKED)
                } else {
                    None
                }
            })
            .ok()
            .and_then(|_| {
                // Take the `Runnable`.
                let runnable = unsafe { self.runnable.with_mut(|r| (*r).take()) };
                assert!(runnable.is_some());

                // Release the lock and signal that the slot is empty.
                self.state.store(0, Release);

                runnable
            })
    }

    fn set(&self, runnable: Runnable) {
        // Take the lock.
        let state = self.state.swap(Self::LOCKED, Acquire);

        // Expect the initial state to be 0. Otherwise, there is already a
        // stored `Runnable` or one is being stored or taken, which should not
        // happen since a task can have at most 1 `Runnable` at a time.
        if state != 0 {
            panic!("Error: there are several live `Runnable`s for the same task");
        }

        // Store the `Runnable`.
        unsafe { self.runnable.with_mut(|r| *r = Some(runnable)) };

        // Release the lock and signal that the slot is populated.
        self.state.store(Self::POPULATED, Release);
    }
}

// An asynchronous count-down counter.
//
// The implementation is intentionally naive and wakes the `CountWatcher` each
// time the count is decremented, even though the future actually only completes
// when the count reaches 0.
//
// Note that for simplicity, the waker may not be changed once set; this is not
// an issue since the tested task implementation never changes the waker.
fn count_down(init_count: usize) -> (CountController, CountWatcher) {
    let inner = Arc::new(CounterInner::new(init_count));

    (
        CountController {
            inner: inner.clone(),
        },
        CountWatcher { inner },
    )
}

// The counter inner type.
struct CounterInner {
    waker: UnsafeCell<Option<Waker>>,
    state: AtomicUsize,
}
impl CounterInner {
    const HAS_WAKER: usize = 1 << 0;
    const INCREMENT: usize = 1 << 1;

    fn new(init_count: usize) -> Self {
        Self {
            waker: UnsafeCell::new(None),
            state: AtomicUsize::new(init_count * Self::INCREMENT),
        }
    }
}

// A `Clone` and `Sync` entity that can decrement the counter.
#[derive(Clone)]
struct CountController {
    inner: Arc<CounterInner>,
}
impl CountController {
    // Decrement the count and notify the counter if a waker is registered.
    //
    // This will panic if the counter is decremented too many times.
    fn decrement(&self) {
        let state = self.inner.state.fetch_sub(CounterInner::INCREMENT, Acquire);

        if state / CounterInner::INCREMENT == 0 {
            panic!("The count-down counter has wrapped around");
        }

        if state & CounterInner::HAS_WAKER != 0 {
            unsafe {
                self.inner
                    .waker
                    .with(|w| (&*w).as_ref().map(Waker::wake_by_ref))
            };
        }
    }
}
unsafe impl Send for CountController {}
unsafe impl Sync for CountController {}

// An entity notified by the controller each time the count is decremented.
struct CountWatcher {
    inner: Arc<CounterInner>,
}
impl Future for CountWatcher {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = self.inner.state.load(Relaxed);

        if state / CounterInner::INCREMENT == 0 {
            return Poll::Ready(());
        }
        if state & CounterInner::HAS_WAKER == CounterInner::HAS_WAKER {
            // Changes of the waker are not supported, so check that the waker
            // indeed hasn't changed.
            assert!(
                unsafe {
                    self.inner
                        .waker
                        .with(|w| cx.waker().will_wake((*w).as_ref().unwrap()))
                },
                "This testing primitive does not support changes of waker"
            );

            return Poll::Pending;
        }

        unsafe { self.inner.waker.with_mut(|w| *w = Some(cx.waker().clone())) };

        let state = self.inner.state.fetch_or(CounterInner::HAS_WAKER, Release);
        if state / CounterInner::INCREMENT == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
unsafe impl Send for CountWatcher {}

#[test]
fn loom_task_schedule() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();
        lazy_static! {
            static ref READY: AtomicBool = AtomicBool::new(false);
        }

        let (promise, runnable, _cancel_token) = spawn(async move { 42 }, schedule_task, ());

        let t = thread::spawn(move || {
            // The task should complete immediately when ran.
            runnable.run();
            READY.store(true, Release);
        });

        if READY.load(Acquire) {
            assert_eq!(promise.poll(), Stage::Ready(42));
        }

        t.join().unwrap();
    });
}

#[test]
fn loom_task_cancel() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();
        lazy_static! {
            static ref IS_CANCELLED: AtomicBool = AtomicBool::new(false);
        }

        let (count_controller, count_watcher) = count_down(1);

        let (promise, runnable, cancel_token) =
            spawn(async move { count_watcher.await }, schedule_task, ());
        runnable.run();

        let waker_thread = thread::spawn(move || {
            count_controller.decrement();
        });
        let scheduler_thread = thread::spawn(|| {
            try_poll_task();
        });
        let cancel_thread = thread::spawn(move || {
            cancel_token.cancel();
            IS_CANCELLED.store(true, Release);
        });

        if IS_CANCELLED.load(Acquire) {
            assert!(promise.poll() != Stage::Pending);
        }

        waker_thread.join().unwrap();
        scheduler_thread.join().unwrap();
        cancel_thread.join().unwrap();
    });
}

#[test]
fn loom_task_run_and_drop() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();

        let (count_controller, count_watcher) = count_down(1);

        let (runnable, cancel_token) =
            spawn_and_forget(async move { count_watcher.await }, schedule_task, ());
        runnable.run();

        let waker_thread = thread::spawn(move || {
            count_controller.decrement();
        });
        let runnable_thread = thread::spawn(|| {
            try_poll_task();
        });
        drop(cancel_token);

        waker_thread.join().unwrap();
        runnable_thread.join().unwrap();
    });
}

#[test]
fn loom_task_run_and_cancel() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();

        let (count_controller, count_watcher) = count_down(1);

        let (runnable, cancel_token) =
            spawn_and_forget(async move { count_watcher.await }, schedule_task, ());
        runnable.run();

        let waker_thread = thread::spawn(move || {
            count_controller.decrement();
        });
        let runnable_thread = thread::spawn(|| {
            try_poll_task();
        });
        cancel_token.cancel();

        waker_thread.join().unwrap();
        runnable_thread.join().unwrap();
    });
}

#[test]
fn loom_task_drop_all() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();

        let (promise, runnable, cancel_token) = spawn(async move {}, schedule_task, ());

        let promise_thread = thread::spawn(move || {
            drop(promise);
        });
        let runnable_thread = thread::spawn(move || {
            drop(runnable);
        });
        drop(cancel_token);

        promise_thread.join().unwrap();
        runnable_thread.join().unwrap();
    });
}

#[test]
fn loom_task_drop_with_waker() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(move || {
        test_prelude!();

        let (count_controller, count_watcher) = count_down(1);

        let (promise, runnable, cancel_token) =
            spawn(async move { count_watcher.await }, schedule_task, ());
        runnable.run();

        let waker_thread = thread::spawn(move || {
            count_controller.decrement();
        });

        let promise_thread = thread::spawn(move || {
            drop(promise);
        });
        let runnable_thread = thread::spawn(|| {
            try_cancel_task(); // drop the runnable if available
        });
        drop(cancel_token);

        waker_thread.join().unwrap();
        promise_thread.join().unwrap();
        runnable_thread.join().unwrap();
    });
}

#[test]
fn loom_task_wake_single_thread() {
    const DEFAULT_PREEMPTION_BOUND: usize = 3;
    const TICK_COUNT1: usize = 4;
    const TICK_COUNT2: usize = 0;

    loom_task_wake(DEFAULT_PREEMPTION_BOUND, TICK_COUNT1, TICK_COUNT2);
}

#[test]
fn loom_task_wake_multi_thread() {
    const DEFAULT_PREEMPTION_BOUND: usize = 3;
    const TICK_COUNT1: usize = 1;
    const TICK_COUNT2: usize = 2;

    loom_task_wake(DEFAULT_PREEMPTION_BOUND, TICK_COUNT1, TICK_COUNT2);
}

// Test task wakening from one or two threads.
fn loom_task_wake(preemption_bound: usize, tick_count1: usize, tick_count2: usize) {
    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(preemption_bound);
    }

    let total_tick_count = tick_count1 + tick_count2;
    builder.check(move || {
        test_prelude!();
        lazy_static! {
            static ref POLL_COUNT: AtomicUsize = AtomicUsize::new(0);
        }

        let (count_controller1, count_watcher) = count_down(total_tick_count);
        let count_controller2 = count_controller1.clone();

        let (promise, runnable, _cancel_token) =
            spawn(async move { count_watcher.await }, schedule_task, ());
        runnable.run();

        let waker_thread1 = if tick_count1 != 0 {
            Some(thread::spawn(move || {
                for _ in 0..tick_count1 {
                    count_controller1.decrement();
                }
            }))
        } else {
            None
        };
        let waker_thread2 = if tick_count2 != 0 {
            Some(thread::spawn(move || {
                for _ in 0..tick_count2 {
                    count_controller2.decrement();
                }
            }))
        } else {
            None
        };
        let scheduler_thread = thread::spawn(move || {
            // Try to run scheduled runnables.
            for _ in 0..total_tick_count {
                if try_poll_task() {
                    POLL_COUNT.fetch_add(1, Release);
                }
            }
        });

        let poll_count = POLL_COUNT.load(Acquire);
        let has_completed = poll_count == total_tick_count;

        // Check that the promise is available if the task has been polled
        // `total_tick_count` times.
        if has_completed {
            assert_eq!(promise.poll(), Stage::Ready(()));
        }

        scheduler_thread.join().unwrap();
        waker_thread1.map(|t| t.join().unwrap());
        waker_thread2.map(|t| t.join().unwrap());

        // If the promise has not been retrieved yet, retrieve it now. It may be
        // necessary to poll the task one last time.
        if !has_completed {
            if POLL_COUNT.load(Acquire) != total_tick_count {
                try_poll_task();
            }

            assert_eq!(promise.poll(), Stage::Ready(()));
        }
    });
}
