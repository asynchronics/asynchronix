use std::thread::LocalKey;

use std::cell::Cell;
use std::marker;
use std::ptr;

/// Declare a new thread-local storage scoped key of type `ScopedKey<T>`.
///
/// This is based on the `scoped-tls` crate, with slight modifications, such as
/// the addition of a `ScopedLocalKey::unset` method and the use of a `map`
/// method that returns `Option::None` when the value is not set, rather than
/// panicking as `with` would.
macro_rules! scoped_thread_local {
    ($(#[$attrs:meta])* $vis:vis static $name:ident: $ty:ty) => (
        $(#[$attrs])*
        $vis static $name: $crate::macros::scoped_thread_local::ScopedLocalKey<$ty>
            = unsafe {
                ::std::thread_local!(static FOO: ::std::cell::Cell<*const ()> = const {
                        ::std::cell::Cell::new(::std::ptr::null())
                });
                $crate::macros::scoped_thread_local::ScopedLocalKey::new(&FOO)
            };
    )
}
pub(crate) use scoped_thread_local;

/// Type representing a thread local storage key corresponding to a reference
/// to the type parameter `T`.
pub(crate) struct ScopedLocalKey<T> {
    inner: &'static LocalKey<Cell<*const ()>>,
    _marker: marker::PhantomData<T>,
}

unsafe impl<T> Sync for ScopedLocalKey<T> {}

impl<T> ScopedLocalKey<T> {
    #[doc(hidden)]
    /// # Safety
    ///
    /// Should only be called through the public macro.
    pub(crate) const unsafe fn new(inner: &'static LocalKey<Cell<*const ()>>) -> Self {
        Self {
            inner,
            _marker: marker::PhantomData,
        }
    }

    /// Inserts a value into this scoped thread local storage slot for the
    /// duration of a closure.
    pub(crate) fn set<F, R>(&'static self, t: &T, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        struct Reset {
            key: &'static LocalKey<Cell<*const ()>>,
            val: *const (),
        }

        impl Drop for Reset {
            fn drop(&mut self) {
                self.key.with(|c| c.set(self.val));
            }
        }

        let prev = self.inner.with(|c| {
            let prev = c.get();
            c.set(t as *const _ as *const ());
            prev
        });

        let _reset = Reset {
            key: self.inner,
            val: prev,
        };

        f()
    }

    /// Removes the value from this scoped thread local storage slot for the
    /// duration of a closure.
    pub(crate) fn unset<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        struct Reset {
            key: &'static LocalKey<Cell<*const ()>>,
            val: *const (),
        }

        impl Drop for Reset {
            fn drop(&mut self) {
                self.key.with(|c| c.set(self.val));
            }
        }

        let prev = self.inner.with(|c| {
            let prev = c.get();
            c.set(ptr::null());
            prev
        });

        let _reset = Reset {
            key: self.inner,
            val: prev,
        };

        f()
    }

    /// Evaluates a closure taking as argument a reference to the value if set
    /// and returns the closures output, or `None` if the value is not set.
    pub(crate) fn map<F, R>(&'static self, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        let val = self.inner.with(|c| c.get());

        if val.is_null() {
            None
        } else {
            Some(f(unsafe { &*(val as *const T) }))
        }
    }
}

#[cfg(all(test, not(nexosim_loom)))]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc::{channel, Sender};
    use std::thread;

    scoped_thread_local!(static FOO: u32);

    #[test]
    fn scoped_local_key_smoke() {
        scoped_thread_local!(static BAR: u32);

        BAR.set(&1, || {
            BAR.map(|_slot| {}).unwrap();
        });
    }

    #[test]
    fn scoped_local_key_set() {
        scoped_thread_local!(static BAR: Cell<u32>);

        BAR.set(&Cell::new(1), || {
            BAR.map(|slot| {
                assert_eq!(slot.get(), 1);
            })
            .unwrap();
        });
    }

    #[test]
    fn scoped_local_key_unset() {
        scoped_thread_local!(static BAR: Cell<u32>);

        BAR.set(&Cell::new(1), || {
            BAR.unset(|| assert!(BAR.map(|_| {}).is_none()));
            BAR.map(|slot| {
                assert_eq!(slot.get(), 1);
            })
            .unwrap();
        });
    }

    #[test]
    fn scoped_local_key_panic_resets() {
        struct Check(Sender<u32>);
        impl Drop for Check {
            fn drop(&mut self) {
                FOO.map(|r| {
                    self.0.send(*r).unwrap();
                })
                .unwrap()
            }
        }

        let (tx, rx) = channel();
        let t = thread::spawn(|| {
            FOO.set(&1, || {
                let _r = Check(tx);

                FOO.set(&2, || panic!());
            });
        });

        assert_eq!(rx.recv().unwrap(), 1);
        assert!(t.join().is_err());
    }
}
