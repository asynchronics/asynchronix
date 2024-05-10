#[cfg(asynchronix_loom)]
#[allow(unused_imports)]
pub(crate) mod sync {
    pub(crate) use loom::sync::{Arc, LockResult, Mutex, MutexGuard};
    pub(crate) use std::sync::PoisonError;

    pub(crate) mod atomic {
        pub(crate) use loom::sync::atomic::{
            fence, AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
        };
    }
}
#[cfg(not(asynchronix_loom))]
#[allow(unused_imports)]
pub(crate) mod sync {
    pub(crate) use std::sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError};

    pub(crate) mod atomic {
        pub(crate) use std::sync::atomic::{
            fence, AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
        };
    }
}

#[cfg(asynchronix_loom)]
pub(crate) mod cell {
    pub(crate) use loom::cell::UnsafeCell;
}
#[cfg(not(asynchronix_loom))]
pub(crate) mod cell {
    #[derive(Debug)]
    pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

    #[allow(dead_code)]
    impl<T> UnsafeCell<T> {
        #[inline(always)]
        pub(crate) fn new(data: T) -> UnsafeCell<T> {
            UnsafeCell(std::cell::UnsafeCell::new(data))
        }
        #[inline(always)]
        pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }
        #[inline(always)]
        pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

#[allow(unused_macros)]
macro_rules! debug_or_loom_assert {
    ($($arg:tt)*) => (if cfg!(any(debug_assertions, asynchronix_loom)) { assert!($($arg)*); })
}
#[allow(unused_macros)]
macro_rules! debug_or_loom_assert_eq {
    ($($arg:tt)*) => (if cfg!(any(debug_assertions, asynchronix_loom)) { assert_eq!($($arg)*); })
}
#[allow(unused_imports)]
pub(crate) use debug_or_loom_assert;
#[allow(unused_imports)]
pub(crate) use debug_or_loom_assert_eq;
