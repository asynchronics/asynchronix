use super::*;

use ::loom::model::Builder;
use ::loom::thread;

// Test adapted from the Tokio test suite.
#[test]
fn loom_queue_basic_steal() {
    const DEFAULT_PREEMPTION_BOUND: usize = 3;
    const LOOP_COUNT: usize = 2;
    const ITEM_COUNT_PER_LOOP: usize = 3;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let worker = Worker::<usize, B4>::new();
        let stealer = worker.stealer();

        let th = thread::spawn(move || {
            let dest_worker = Worker::<usize, B4>::new();
            let mut n = 0;

            for _ in 0..3 {
                if stealer.steal_and_pop(&dest_worker, |n| n - n / 2).is_ok() {
                    n += 1;
                    while dest_worker.pop().is_some() {
                        n += 1;
                    }
                }
            }

            n
        });

        let mut n = 0;

        for _ in 0..LOOP_COUNT {
            for _ in 0..(ITEM_COUNT_PER_LOOP - 1) {
                if worker.push(42).is_err() {
                    n += 1;
                }
            }

            if worker.pop().is_some() {
                n += 1;
            }

            // Push another task
            if worker.push(42).is_err() {
                n += 1;
            }

            while worker.pop().is_some() {
                n += 1;
            }
        }

        n += th.join().unwrap();

        assert_eq!(ITEM_COUNT_PER_LOOP * LOOP_COUNT, n);
    });
}

// Test adapted from the Tokio test suite.
#[test]
fn loom_queue_drain_overflow() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;
    const ITEM_COUNT: usize = 7;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let worker = Worker::<usize, B4>::new();
        let stealer = worker.stealer();

        let th = thread::spawn(move || {
            let dest_worker = Worker::<usize, B4>::new();
            let mut n = 0;

            if stealer.steal_and_pop(&dest_worker, |n| n - n / 2).is_ok() {
                n += 1;
                while dest_worker.pop().is_some() {
                    n += 1;
                }
            }

            n
        });

        let mut n = 0;

        // Push an item, pop an item.
        worker.push(42).unwrap();

        if worker.pop().is_some() {
            n += 1;
        }

        for _ in 0..(ITEM_COUNT - 1) {
            if worker.push(42).is_err() {
                // Spin until some of the old items can be drained to make room
                // for the new item.
                loop {
                    if let Ok(drain) = worker.drain(|n| n - n / 2) {
                        for _ in drain {
                            n += 1;
                        }
                        assert_eq!(worker.push(42), Ok(()));
                        break;
                    }
                    thread::yield_now();
                }
            }
        }

        n += th.join().unwrap();

        while worker.pop().is_some() {
            n += 1;
        }

        assert_eq!(ITEM_COUNT, n);
    });
}

// Test adapted from the Tokio test suite.
#[test]
fn loom_queue_multi_stealer() {
    const DEFAULT_PREEMPTION_BOUND: usize = 3;
    const ITEM_COUNT: usize = 5;

    fn steal_half(stealer: Stealer<usize, B4>) -> usize {
        let dest_worker = Worker::<usize, B4>::new();

        if stealer.steal_and_pop(&dest_worker, |n| n - n / 2).is_ok() {
            let mut n = 1;
            while dest_worker.pop().is_some() {
                n += 1;
            }

            n
        } else {
            0
        }
    }

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let worker = Worker::<usize, B4>::new();
        let stealer1 = worker.stealer();
        let stealer2 = worker.stealer();

        let th1 = thread::spawn(move || steal_half(stealer1));
        let th2 = thread::spawn(move || steal_half(stealer2));

        let mut n = 0;
        for _ in 0..ITEM_COUNT {
            if worker.push(42).is_err() {
                n += 1;
            }
        }

        while worker.pop().is_some() {
            n += 1;
        }

        n += th1.join().unwrap();
        n += th2.join().unwrap();

        assert_eq!(ITEM_COUNT, n);
    });
}

// Test adapted from the Tokio test suite.
#[test]
fn loom_queue_chained_steal() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let w1 = Worker::<usize, B4>::new();
        let w2 = Worker::<usize, B4>::new();
        let s1 = w1.stealer();
        let s2 = w2.stealer();

        for _ in 0..4 {
            w1.push(42).unwrap();
            w2.push(42).unwrap();
        }

        let th = thread::spawn(move || {
            let dest_worker = Worker::<usize, B4>::new();
            let _ = s1.steal_and_pop(&dest_worker, |n| n - n / 2);

            while dest_worker.pop().is_some() {}
        });

        while w1.pop().is_some() {}

        let _ = s2.steal_and_pop(&w1, |n| n - n / 2);

        th.join().unwrap();

        while w1.pop().is_some() {}
        while w2.pop().is_some() {}
    });
}

// A variant of multi-stealer with concurrent push.
#[test]
fn loom_queue_push_and_steal() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    fn steal_half(stealer: Stealer<usize, B4>) -> usize {
        let dest_worker = Worker::<usize, B4>::new();

        if stealer.steal_and_pop(&dest_worker, |n| n - n / 2).is_ok() {
            let mut n = 1;
            while dest_worker.pop().is_some() {
                n += 1;
            }

            n
        } else {
            0
        }
    }

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let worker = Worker::<usize, B4>::new();
        let stealer1 = worker.stealer();
        let stealer2 = worker.stealer();

        let th1 = thread::spawn(move || steal_half(stealer1));
        let th2 = thread::spawn(move || steal_half(stealer2));

        worker.push(42).unwrap();
        worker.push(42).unwrap();

        let mut n = 0;
        while worker.pop().is_some() {
            n += 1;
        }

        n += th1.join().unwrap();
        n += th2.join().unwrap();

        assert_eq!(n, 2);
    });
}

// Attempts extending the queue based on `Worker::free_capacity`.
#[test]
fn loom_queue_extend() {
    const DEFAULT_PREEMPTION_BOUND: usize = 4;

    fn steal_half(stealer: Stealer<usize, B4>) -> usize {
        let dest_worker = Worker::<usize, B4>::new();

        if stealer.steal_and_pop(&dest_worker, |n| n - n / 2).is_ok() {
            let mut n = 1;
            while dest_worker.pop().is_some() {
                n += 1;
            }

            n
        } else {
            0
        }
    }

    let mut builder = Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(DEFAULT_PREEMPTION_BOUND);
    }

    builder.check(|| {
        let worker = Worker::<usize, B4>::new();
        let stealer1 = worker.stealer();
        let stealer2 = worker.stealer();

        let th1 = thread::spawn(move || steal_half(stealer1));
        let th2 = thread::spawn(move || steal_half(stealer2));

        worker.push(1).unwrap();
        worker.push(7).unwrap();

        // Try to fill up the queue.
        let spare_capacity = worker.spare_capacity();
        assert!(spare_capacity >= 2);
        worker.extend(0..spare_capacity);

        let mut n = 0;

        n += th1.join().unwrap();
        n += th2.join().unwrap();

        while worker.pop().is_some() {
            n += 1;
        }

        assert_eq!(2 + spare_capacity, n);
    });
}
