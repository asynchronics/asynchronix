//! An event system ("eventcount") specifically designed for the async case.
//!
//! Eventcount-like primitives are useful to make some operations on a lock-free
//! structure blocking, for instance to transform bounded queues into bounded
//! channels. Such a primitive allows an interested task to block until a
//! predicate is satisfied by checking the predicate each time it receives a
//! notification.
//!
//! This implementation tries to improve on the similar `event_listener` crate
//! by limiting the number of locking operations on the mutex-protected list of
//! notifiers: the lock is typically taken only once for each time a waiter is
//! blocked and once for notifying, thus reducing the need for synchronization
//! operations. Finally, spurious wake-ups are only generated in very rare
//! circumstances.

use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

use crate::loom_exports::cell::UnsafeCell;
use crate::loom_exports::sync::atomic::{self, AtomicBool};
use crate::loom_exports::sync::Mutex;

/// An object that can receive or send notifications.
pub(super) struct Event {
    wait_set: WaitSet,
}

impl Event {
    /// Creates a new event.
    pub(super) fn new() -> Self {
        Self {
            wait_set: WaitSet::default(),
        }
    }

    /// Notify a number of awaiting events that the predicate should be checked.
    ///
    /// If less events than requested are currently awaiting, then all awaiting
    /// event are notified.
    #[inline(always)]
    pub(super) fn notify(&self, n: usize) {
        // This fence synchronizes with the other fence in `WaitUntil::poll` and
        // ensures that either the `poll` method will successfully check the
        // predicate set before this call, or the notifier inserted by `poll`
        // will be visible in the wait list when calling `WaitSet::notify` (or
        // both).
        atomic::fence(Ordering::SeqCst);

        // Safety: all notifiers in the wait set are guaranteed to be alive
        // since the `WaitUntil` drop handler ensures that notifiers are removed
        // from the wait set before they are deallocated.
        unsafe {
            self.wait_set.notify_relaxed(n);
        }
    }

    /// Returns a future that can be `await`ed until the provided predicate is
    /// satisfied.
    pub(super) fn wait_until<F: FnMut() -> Option<T>, T>(&self, predicate: F) -> WaitUntil<F, T> {
        WaitUntil::new(&self.wait_set, predicate)
    }
}

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

/// A waker wrapper that can be inserted in a list.
///
/// A notifier always has an exclusive owner or borrower, except in one edge
/// case: the `WaitSet::remove_relaxed()` method may create a shared reference
/// while the notifier is concurrently accessed under the `wait_set` mutex by
/// one of the `WaitSet` methods. So occasionally 2 references to a `Notifier`
/// will exist at the same time, meaning that even when accessed under the
/// `wait_set` mutex, a notifier can only be accessed by reference.
pub(super) struct Notifier {
    /// The current waker, if any.
    waker: Option<Waker>,
    /// Pointer to the previous wait set notifier.
    prev: UnsafeCell<Option<NonNull<Notifier>>>,
    /// Pointer to the next wait set notifier.
    next: UnsafeCell<Option<NonNull<Notifier>>>,
    /// Flag indicating whether the notifier is currently in the wait set.
    in_wait_set: AtomicBool,
}

impl Notifier {
    /// Creates a new Notifier without any registered waker.
    pub(super) fn new() -> Self {
        Self {
            waker: None,
            prev: UnsafeCell::new(None),
            next: UnsafeCell::new(None),
            in_wait_set: AtomicBool::new(false),
        }
    }

    /// Stores the specified waker if it differs from the cached waker.
    fn set_waker(&mut self, waker: &Waker) {
        if match &self.waker {
            Some(w) => !w.will_wake(waker),
            None => true,
        } {
            self.waker = Some(waker.clone());
        }
    }

    /// Notifies the task.
    fn wake(&self) {
        // Safety: the waker is only ever accessed mutably when the notifier is
        // itself accessed mutably. The caller claims shared (non-mutable)
        // ownership of the notifier, so there is not possible concurrent
        // mutable access to the notifier and therefore to the waker.
        if let Some(w) = &self.waker {
            w.wake_by_ref();
        }
    }
}

unsafe impl Send for Notifier {}
unsafe impl Sync for Notifier {}

/// A future that can be `await`ed until a predicate is satisfied.
pub(super) struct WaitUntil<'a, F: FnMut() -> Option<T>, T> {
    state: WaitUntilState,
    predicate: F,
    wait_set: &'a WaitSet,
}

impl<'a, F: FnMut() -> Option<T>, T> WaitUntil<'a, F, T> {
    /// Creates a future associated with the specified event sink that can be
    /// `await`ed until the specified predicate is satisfied.
    fn new(wait_set: &'a WaitSet, predicate: F) -> Self {
        Self {
            state: WaitUntilState::Uninit,
            predicate,
            wait_set,
        }
    }
}

impl<F: FnMut() -> Option<T>, T> Drop for WaitUntil<'_, F, T> {
    fn drop(&mut self) {
        if let WaitUntilState::Polled(notifier) = self.state {
            // If we are in the `Polled` stated, it means that the future was
            // cancelled and its notifier may still be in the wait set: it is
            // necessary to cancel the notifier so that another event sink can
            // be notified if one is registered, and then to deallocate the
            // notifier.
            //
            // Safety: all notifiers in the wait set are guaranteed to be alive
            // since this drop handler ensures that notifiers are removed from
            // the wait set before they are deallocated. After the notifier is
            // removed from the list we can claim unique ownership and
            // deallocate the notifier.
            unsafe {
                self.wait_set.cancel(notifier);
                let _ = Box::from_raw(notifier.as_ptr());
            }
        }
    }
}

impl<'a, F: FnMut() -> Option<T>, T> Unpin for WaitUntil<'a, F, T> {}

unsafe impl<F: (FnMut() -> Option<T>) + Send, T: Send> Send for WaitUntil<'_, F, T> {}

impl<'a, F: FnMut() -> Option<T>, T> Future for WaitUntil<'a, F, T> {
    type Output = T;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(self.state != WaitUntilState::Completed);

        // Remove the notifier if it is in the wait set. In most cases this will
        // be a cheap no-op because, unless the wake-up is spurious, the
        // notifier was already removed from the wait set.
        //
        // Removing the notifier before checking the predicate is necessary to
        // avoid races such as this one:
        //
        // 1) event sink A unsuccessfully checks the predicate, inserts its
        //    notifier in the wait set, unsuccessfully re-checks the predicate,
        //    returns `Poll::Pending`,
        // 2) event sink B unsuccessfully checks the predicate, inserts its
        //    notifier in the wait set, unsuccessfully re-checks the predicate,
        //    returns `Poll::Pending`,
        // 3) the event source makes one predicate satisfiable,
        // 4) event sink A is spuriously awaken and successfully checks the
        //    predicates, returns `Poll::Ready`,
        // 5) the event source notifies event sink B,
        // 6) event sink B is awaken and unsuccessfully checks the predicate,
        //    inserts its notifier in the wait set, unsuccessfully re-checks the
        //    predicate, returns `Poll::Pending`,
        // 7) the event source makes another predicate satisfiable.
        // 8) if now the notifier of event sink A was not removed from the wait
        //    set, the event source may notify event sink A (which is no longer
        //    interested) rather than event sink B, meaning that event sink B
        //    will never be notified.
        if let WaitUntilState::Polled(notifier) = self.state {
            // Safety: all notifiers in the wait set are guaranteed to be alive
            // since the `WaitUntil` drop handler ensures that notifiers are
            // removed from the wait set before they are deallocated. Using the
            // relaxed version of `notify` is enough since the notifier was
            // inserted in the same future so there exists a happen-before
            // relationship with the insertion operation.
            unsafe { self.wait_set.remove_relaxed(notifier) };
        }

        // Fast path.
        if let Some(v) = (self.predicate)() {
            if let WaitUntilState::Polled(notifier) = self.state {
                // Safety: the notifier is no longer in the wait set so we can
                // claim unique ownership and deallocate the notifier.
                let _ = unsafe { Box::from_raw(notifier.as_ptr()) };
            }

            self.state = WaitUntilState::Completed;

            return Poll::Ready(v);
        }

        let mut notifier = if let WaitUntilState::Polled(notifier) = self.state {
            notifier
        } else {
            unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(Notifier::new()))) }
        };

        // Set or update the notifier.
        //
        // Safety: the notifier is not (or no longer) in the wait list so we
        // have exclusive ownership.
        let waker = cx.waker();
        unsafe { notifier.as_mut().set_waker(waker) };

        // Safety: all notifiers in the wait set are guaranteed to be alive
        // since the `WaitUntil` drop handler ensures that notifiers are removed
        // from the wait set before they are deallocated.
        unsafe { self.wait_set.insert(notifier) };

        // This fence synchronizes with the other fence in `Event::notify` and
        // ensures that either the predicate below will be satisfied or the
        // event source will see the notifier inserted above in the wait list
        // after it makes the predicate satisfiable (or both).
        atomic::fence(Ordering::SeqCst);

        if let Some(v) = (self.predicate)() {
            // We need to cancel and not merely remove the notifier from the
            // wait set so that another event sink can be notified in case we
            // have been notified just after checking the predicate. This is an
            // example of race that makes this necessary:
            //
            // 1) event sink A and event sink B both unsuccessfully check the
            //    predicate,
            // 2) the event source makes one predicate satisfiable and tries to
            //    notify an event sink but fails since no notifier has been
            //    inserted in the wait set yet,
            // 3) event sink A and event sink B both insert their notifier in
            //    the wait set,
            // 4) event sink A re-checks the predicate, successfully,
            // 5) event sink B re-checks the predicate, unsuccessfully,
            // 6) the event source makes another predicate satisfiable,
            // 7) the event source sends a notification for the second predicate
            //    but unfortunately chooses the "wrong" notifier in the wait
            //    set, i.e. that of event sink A -- note that this is always
            //    possible irrespective of FIFO or LIFO ordering because it also
            //    depends on the order of notifier insertion in step 3)
            // 8) if, before returning, event sink A merely removes itself from
            //    the wait set without notifying another event sink, then event
            //    sink B will never be notified.
            //
            // Safety: all notifiers in the wait set are guaranteed to be alive
            // since the `WaitUntil` drop handler ensures that notifiers are
            // removed from the wait set before they are deallocated.
            unsafe {
                self.wait_set.cancel(notifier);
            }

            self.state = WaitUntilState::Completed;

            // Safety: the notifier is not longer in the wait set so we can
            // claim unique ownership and deallocate the notifier.
            let _ = unsafe { Box::from_raw(notifier.as_ptr()) };

            return Poll::Ready(v);
        }

        self.state = WaitUntilState::Polled(notifier);

        Poll::Pending
    }
}

/// State of the `WaitUntil` future.
#[derive(PartialEq)]
enum WaitUntilState {
    Uninit,
    Polled(NonNull<Notifier>),
    Completed,
}

/// A set of notifiers.
///
/// The set wraps a Mutex-protected list of notifiers and manages a flag for
/// fast assessment of list emptiness.
struct WaitSet {
    list: Mutex<List>,
    is_empty: AtomicBool,
}

impl WaitSet {
    /// Inserts a node in the wait set.
    ///
    /// # Safety
    ///
    /// The specified notifier and all notifiers in the wait set must be alive.
    /// The notifier should not be already in the wait set.
    unsafe fn insert(&self, notifier: NonNull<Notifier>) {
        let mut list = self.list.lock().unwrap();

        #[cfg(any(debug_assertions, tachyonix_loom))]
        if notifier.as_ref().in_wait_set.load(Ordering::Relaxed) {
            drop(list); // avoids poisoning the lock
            panic!("the notifier was already in the wait set");
        }

        // Orderings: Relaxed ordering is sufficient since before this point the
        // notifier was not in the list and therefore not shared.
        notifier.as_ref().in_wait_set.store(true, Ordering::Relaxed);

        list.push_back(notifier);

        // Ordering: since this flag is only ever mutated within the
        // mutex-protected critical section, Relaxed ordering is sufficient.
        self.is_empty.store(false, Ordering::Relaxed);
    }

    /// Remove the specified notifier if it is still in the wait set.
    ///
    /// After a call to `remove`, the caller is guaranteed that the wait set
    /// will no longer access the specified notifier.
    ///
    /// Note that for performance reasons, the presence of the notifier in the
    /// list is checked without acquiring the lock. This fast check will never
    /// lead to a notifier staying in the list as long as there exists an
    /// happens-before relationship between this call and the earlier call to
    /// `insert`. A happens-before relationship always exists if these calls are
    /// made on the same thread or across `await` points.
    ///
    /// # Safety
    ///
    /// The specified notifier and all notifiers in the wait set must be alive.
    /// This function may fail to remove the notifier if a happens-before
    /// relationship does not exist with the previous call to `insert`.
    unsafe fn remove_relaxed(&self, notifier: NonNull<Notifier>) {
        // Preliminarily check whether the notifier is already in the list (fast
        // path).
        //
        // This is the only instance where the `in_wait_set` flag is accessed
        // outside the mutex-protected critical section while the notifier may
        // still be in the list. The only risk is that the load will be stale
        // and will read `true` even though the notifier is no longer in the
        // list, but this is not an issue since in that case the actual state
        // will be checked again after taking the lock.
        //
        // Ordering: Acquire synchronizes with the `Release` orderings in the
        // `notify` and `cancel` methods; it is necessary to ensure that the
        // waker is no longer in use by the wait set and can therefore be
        // modified after returning from `remove`.
        let in_wait_set = notifier.as_ref().in_wait_set.load(Ordering::Acquire);
        if !in_wait_set {
            return;
        }

        self.remove(notifier);
    }

    /// Remove the specified notifier if it is still in the wait set.
    ///
    /// After a call to `remove`, the caller is guaranteed that the wait set
    /// will no longer access the specified notifier.
    ///
    /// # Safety
    ///
    /// The specified notifier and all notifiers in the wait set must be alive.
    unsafe fn remove(&self, notifier: NonNull<Notifier>) {
        let mut list = self.list.lock().unwrap();

        // Check again whether the notifier is already in the list
        //
        // Ordering: since this flag is only ever mutated within the
        // mutex-protected critical section and since the wait set also accesses
        // the waker only in the critical section, even with Relaxed ordering it
        // is guaranteed that if `in_wait_set` reads `false` then the waker is
        // no longer in use by the wait set.
        let in_wait_set = notifier.as_ref().in_wait_set.load(Ordering::Relaxed);
        if !in_wait_set {
            return;
        }

        list.remove(notifier);
        if list.is_empty() {
            // Ordering: since this flag is only ever mutated within the
            // mutex-protected critical section, Relaxed ordering is sufficient.
            self.is_empty.store(true, Ordering::Relaxed);
        }

        // Ordering: this flag is only ever mutated within the mutex-protected
        // critical section and since the waker is not accessed in this method,
        // it does not need to synchronize with a later call to `remove`;
        // therefore, Relaxed ordering is sufficient.
        notifier
            .as_ref()
            .in_wait_set
            .store(false, Ordering::Relaxed);
    }

    /// Remove the specified notifier if it is still in the wait set, otherwise
    /// notify another event sink.
    ///
    /// After a call to `cancel`, the caller is guaranteed that the wait set
    /// will no longer access the specified notifier.
    ///
    /// # Safety
    ///
    /// The specified notifier and all notifiers in the wait set must be alive.
    /// Wakers of notifiers which pointer is in the wait set may not be accessed
    /// mutably.
    unsafe fn cancel(&self, notifier: NonNull<Notifier>) {
        let mut list = self.list.lock().unwrap();

        let in_wait_set = notifier.as_ref().in_wait_set.load(Ordering::Relaxed);
        if in_wait_set {
            list.remove(notifier);
            if list.is_empty() {
                self.is_empty.store(true, Ordering::Relaxed);
            }

            // Ordering: this flag is only ever mutated within the
            // mutex-protected critical section and since the waker is not
            // accessed, it does not need to synchronize with the Acquire load
            // in the `remove` method; therefore, Relaxed ordering is
            // sufficient.
            notifier
                .as_ref()
                .in_wait_set
                .store(false, Ordering::Relaxed);
        } else if let Some(other_notifier) = list.pop_front() {
            // Safety: the waker can be accessed by reference because the
            // event sink is not allowed to access the waker mutably before
            // `in_wait_set` is cleared.
            other_notifier.as_ref().wake();

            // Ordering: the Release memory ordering synchronizes with the
            // Acquire ordering in the `remove` method; it is required to
            // ensure that once `in_wait_set` reads `false` (using Acquire
            // ordering), the waker is no longer in use by the wait set and
            // can therefore be modified.
            other_notifier
                .as_ref()
                .in_wait_set
                .store(false, Ordering::Release);
        }
    }

    /// Send a notification to `count` notifiers within the wait set, or to all
    /// notifiers if the wait set contains less than `count` notifiers.
    ///
    /// Note that for performance reasons, list emptiness is checked without
    /// acquiring the wait set lock. Therefore, in order to prevent the
    /// possibility that a wait set is seen as empty when it isn't, external
    /// synchronization is required to make sure that all side effects of a
    /// previous call to `insert` are fully visible. For instance, an atomic
    /// memory fence maye be placed before this call and another one after the
    /// insertion of a notifier.
    ///
    /// # Safety
    ///
    /// All notifiers in the wait set must be alive. Wakers of notifiers which
    /// pointer is in the wait set may not be accessed mutably.
    #[inline(always)]
    unsafe fn notify_relaxed(&self, count: usize) {
        let is_empty = self.is_empty.load(Ordering::Relaxed);
        if is_empty {
            return;
        }

        self.notify(count);
    }

    /// Send a notification to `count` notifiers within the wait set, or to all
    /// notifiers if the wait set contains less than `count` notifiers.
    ///
    /// # Safety
    ///
    /// All notifiers in the wait set must be alive. Wakers of notifiers which
    /// pointer is in the wait set may not be accessed mutably.
    unsafe fn notify(&self, count: usize) {
        let mut list = self.list.lock().unwrap();
        for _ in 0..count {
            let notifier = {
                if let Some(notifier) = list.pop_front() {
                    if list.is_empty() {
                        self.is_empty.store(true, Ordering::Relaxed);
                    }
                    notifier
                } else {
                    return;
                }
            };

            // Note: the event sink must be notified before the end of the
            // mutex-protected critical section. Otherwise, a concurrent call to
            // `remove` could succeed in taking the lock before the waker has
            // been called, and seeing that the notifier is no longer in the
            // list would lead its caller to believe that it has now sole
            // ownership on the notifier even though the call to `wake` has yet
            // to be made.
            //
            // Safety: the waker can be accessed by reference since the event
            // sink is not allowed to access the waker mutably before
            // `in_wait_set` is cleared.
            notifier.as_ref().wake();

            // Ordering: the Release memory ordering synchronizes with the
            // Acquire ordering in the `remove` method; it is required to ensure
            // that once `in_wait_set` reads `false` (using Acquire ordering),
            // the waker can be safely modified.
            notifier
                .as_ref()
                .in_wait_set
                .store(false, Ordering::Release);
        }
    }
}

impl Default for WaitSet {
    fn default() -> Self {
        Self {
            list: Default::default(),
            is_empty: AtomicBool::new(true),
        }
    }
}

#[derive(Default)]
struct List {
    front: Option<NonNull<Notifier>>,
    back: Option<NonNull<Notifier>>,
}

impl List {
    /// Inserts a node at the back of the list.
    ///
    /// # Safety
    ///
    /// The provided notifier and all notifiers which pointer is in the list
    /// must be alive.
    unsafe fn push_back(&mut self, notifier: NonNull<Notifier>) {
        // Safety: the `prev` and `next` pointers are only be accessed when the
        // list is locked.
        let old_back = mem::replace(&mut self.back, Some(notifier));
        match old_back {
            None => self.front = Some(notifier),
            Some(prev) => prev.as_ref().next.with_mut(|n| *n = Some(notifier)),
        }

        // Link the new notifier.
        let notifier = notifier.as_ref();
        notifier.prev.with_mut(|n| *n = old_back);
        notifier.next.with_mut(|n| *n = None);
    }

    /// Removes and returns the notifier at the front of the list, if any.
    ///
    /// # Safety
    ///
    /// All notifiers which pointer is in the list must be alive.
    unsafe fn pop_front(&mut self) -> Option<NonNull<Notifier>> {
        let notifier = self.front?;

        // Unlink from the next notifier.
        let next = notifier.as_ref().next.with(|n| *n);
        self.front = next;
        match next {
            None => self.back = None,
            Some(next) => next.as_ref().prev.with_mut(|n| *n = None),
        }

        Some(notifier)
    }

    /// Removes the specified notifier.
    ///
    /// # Safety
    ///
    /// The specified notifier and all notifiers which pointer is in the list
    /// must be alive.
    unsafe fn remove(&mut self, notifier: NonNull<Notifier>) {
        // Unlink from the previous and next notifiers.
        let prev = notifier.as_ref().prev.with(|n| *n);
        let next = notifier.as_ref().next.with(|n| *n);
        match prev {
            None => self.front = next,
            Some(prev) => prev.as_ref().next.with_mut(|n| *n = next),
        }
        match next {
            None => self.back = prev,
            Some(next) => next.as_ref().prev.with_mut(|n| *n = prev),
        }
    }

    /// Returns `true` if the list is empty.
    fn is_empty(&self) -> bool {
        self.front.is_none()
    }
}

/// Loom tests.
#[cfg(all(test, asynchronix_loom))]
mod tests {
    use super::*;

    use std::future::Future;
    use std::marker::PhantomPinned;
    use std::task::{Context, Poll};

    use loom::model::Builder;
    use loom::sync::atomic::AtomicUsize;
    use loom::sync::Arc;
    use loom::thread;

    use waker_fn::waker_fn;

    /// A waker factory that accepts notifications from the newest waker only.
    #[derive(Clone, Default)]
    struct MultiWaker {
        state: Arc<AtomicUsize>,
    }

    impl MultiWaker {
        /// Clears the notification flag.
        ///
        /// This operation has unconditional Relaxed semantic and for this
        /// reason should be used instead of `take_notification` when the intent
        /// is only to cancel a notification for book-keeping purposes, e.g. to
        /// simulate a spurious wake-up, without introducing unwanted
        /// synchronization.
        fn clear_notification(&self) {
            self.state.fetch_and(!1, Ordering::Relaxed);
        }

        /// Clears the notification flag and returns the former notification
        /// status.
        ///
        /// This operation has Acquire semantic when a notification is indeed
        /// present, and Relaxed otherwise. It is therefore appropriate to
        /// simulate a scheduler receiving a notification as it ensures that all
        /// memory operations preceding the notification of a task are visible.
        fn take_notification(&self) -> bool {
            // Clear the notification flag.
            let mut state = self.state.load(Ordering::Relaxed);
            loop {
                let notified_stated = state | 1;
                let unnotified_stated = state & !1;
                match self.state.compare_exchange_weak(
                    notified_stated,
                    unnotified_stated,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(s) => {
                        state = s;
                        if state == unnotified_stated {
                            return false;
                        }
                    }
                }
            }
        }

        /// Clears the notification flag and creates a new waker.
        fn new_waker(&self) -> Waker {
            // Increase the epoch and clear the notification flag.
            let mut state = self.state.load(Ordering::Relaxed);
            let mut epoch;
            loop {
                // Increase the epoch by 2.
                epoch = (state & !1) + 2;
                match self.state.compare_exchange_weak(
                    state,
                    epoch,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(s) => state = s,
                }
            }

            // Create a waker that only notifies if it is the newest waker.
            let waker_state = self.state.clone();
            waker_fn(move || {
                let mut state = waker_state.load(Ordering::Relaxed);
                loop {
                    let new_state = if state & !1 == epoch {
                        epoch | 1
                    } else {
                        break;
                    };
                    match waker_state.compare_exchange(
                        state,
                        new_state,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(s) => state = s,
                    }
                }
            })
        }
    }

    /// A simple counter that can be used to simulate the availability of a
    /// certain number of tokens. In order to model the weakest possible
    /// predicate from the viewpoint of atomic memory ordering, only Relaxed
    /// atomic operations are used.
    #[derive(Default)]
    struct Counter {
        count: AtomicUsize,
    }

    impl Counter {
        fn increment(&self) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn try_decrement(&self) -> Option<()> {
            let mut count = self.count.load(Ordering::Relaxed);
            loop {
                if count == 0 {
                    return None;
                }
                match self.count.compare_exchange(
                    count,
                    count - 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(()),
                    Err(c) => count = c,
                }
            }
        }
    }

    /// A closure that contains the targets of all references captured by a
    /// `WaitUntil` Future.
    ///
    /// This ugly thing is needed to arbitrarily extend the lifetime of a
    /// `WaitUntil` future and thus mimic the behavior of an executor task.
    struct WaitUntilClosure {
        event: Arc<Event>,
        token_counter: Arc<Counter>,
        wait_until: Option<Box<dyn Future<Output = ()>>>,
        _pin: PhantomPinned,
    }

    impl WaitUntilClosure {
        /// Creates a `WaitUntil` future embedded together with the targets
        /// captured by reference.
        fn new(event: Arc<Event>, token_counter: Arc<Counter>) -> Pin<Box<Self>> {
            let res = Self {
                event,
                token_counter,
                wait_until: None,
                _pin: PhantomPinned,
            };
            let boxed = Box::new(res);

            // Artificially extend the lifetimes of the captured references.
            let event_ptr = &*boxed.event as *const Event;
            let token_counter_ptr = &boxed.token_counter as *const Arc<Counter>;

            // Safety: we now commit to never move the closure and to ensure
            // that the `WaitUntil` future does not outlive the captured
            // references.
            let wait_until: Box<dyn Future<Output = _>> = unsafe {
                Box::new((*event_ptr).wait_until(move || (*token_counter_ptr).try_decrement()))
            };
            let mut pinned_box: Pin<Box<WaitUntilClosure>> = boxed.into();

            let mut_ref: Pin<&mut Self> = Pin::as_mut(&mut pinned_box);
            unsafe {
                // This is safe: we are not moving the closure.
                Pin::get_unchecked_mut(mut_ref).wait_until = Some(wait_until);
            }

            pinned_box
        }

        /// Returns a pinned, type-erased `WaitUntil` future.
        fn as_pinned_future(self: Pin<&mut Self>) -> Pin<&mut dyn Future<Output = ()>> {
            unsafe { self.map_unchecked_mut(|s| s.wait_until.as_mut().unwrap().as_mut()) }
        }
    }

    impl Drop for WaitUntilClosure {
        fn drop(&mut self) {
            // Make sure that the `WaitUntil` future does not outlive its
            // captured references.
            self.wait_until = None;
        }
    }

    /// An enum that registers the final state of a `WaitUntil` future at the
    /// completion of a thread.
    ///
    /// When the future is still in a `Polled` state, this future is moved into
    /// the enum so as to extend its lifetime and allow it to be further
    /// notified.
    enum FutureState {
        Completed,
        Polled(Pin<Box<WaitUntilClosure>>),
        Cancelled,
    }

    /// Make a certain amount of tokens available and notify as many waiters
    /// among all registered waiters. Optionally, it is possible to:
    /// - request that `max_spurious_wake` threads will simulate a spurious
    ///   wake-up if the waiter is polled and returns `Poll::Pending`,
    /// - request that `max_cancellations` threads will cancel the waiter if
    ///   the waiter is polled and returns `Poll::Pending`,
    /// - change the waker each time it is polled.
    ///
    /// Note that the aggregate number of specified cancellations and spurious
    /// wake-ups cannot exceed the number of waiters.
    fn loom_event_notify(
        token_count: usize,
        waiter_count: usize,
        max_spurious_wake: usize,
        max_cancellations: usize,
        change_waker: bool,
        preemption_bound: usize,
    ) {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(preemption_bound);
        }

        builder.check(move || {
            let token_counter = Arc::new(Counter::default());
            let event = Arc::new(Event::new());

            let mut wakers: Vec<MultiWaker> = Vec::new();
            wakers.resize_with(waiter_count, Default::default);

            let waiter_threads: Vec<_> = wakers
                .iter()
                .enumerate()
                .map(|(i, multi_waker)| {
                    thread::spawn({
                        let multi_waker = multi_waker.clone();
                        let mut wait_until =
                            WaitUntilClosure::new(event.clone(), token_counter.clone());

                        move || {
                            // `max_cancellations` threads will cancel the
                            // waiter if the waiter returns `Poll::Pending`.
                            let cancel_waiter = i < max_cancellations;
                            // `max_spurious_wake` threads will simulate a
                            // spurious wake-up if the waiter returns
                            // `Poll::Pending`.
                            let mut spurious_wake = i >= max_cancellations
                                && i < (max_cancellations + max_spurious_wake);

                            let mut waker = multi_waker.new_waker();
                            loop {
                                let mut cx = Context::from_waker(&waker);
                                let poll_state =
                                    wait_until.as_mut().as_pinned_future().poll(&mut cx);

                                // Return successfully if the predicate was
                                // checked successfully.
                                if matches!(poll_state, Poll::Ready(_)) {
                                    return FutureState::Completed;
                                }

                                // The future has returned Poll::Pending.
                                // Depending on the situation, we will either
                                // cancel the future, return and wait for a
                                // notification, or poll again.

                                if cancel_waiter {
                                    // The `wait_until` future is dropped while
                                    // in pending state, which simulates future
                                    // cancellation. Note that the notification
                                    // was intentionally cleared earlier so the
                                    // task will not be counted as a task that
                                    // should eventually succeed.
                                    return FutureState::Cancelled;
                                }
                                if spurious_wake {
                                    // Clear the notification, if any.
                                    multi_waker.clear_notification();
                                } else if !multi_waker.take_notification() {
                                    // The async runtime would normally keep the
                                    // `wait_until` future alive after `poll`
                                    // returns `Pending`. This behavior is
                                    // emulated by returning the `WaitUntil`
                                    // closure from the thread so as to extend
                                    // it lifetime.
                                    return FutureState::Polled(wait_until);
                                }

                                // The task was notified or spuriously awaken.
                                spurious_wake = false;
                                if change_waker {
                                    waker = multi_waker.new_waker();
                                }
                            }
                        }
                    })
                })
                .collect();

            // Increment the token count and notify a consumer after each
            // increment.
            for _ in 0..token_count {
                token_counter.increment();
                event.notify(1);
            }

            // Join all threads and check which of them have successfully
            // checked the predicate. It is important that all `FutureState`
            // returned by the threads be kept alive until _all_ threads have
            // joined because `FutureState::Polled` items extend the lifetime of
            // their future so they can still be notified.
            let future_state: Vec<_> = waiter_threads
                .into_iter()
                .map(|th| th.join().unwrap())
                .collect();

            // See which threads have successfully completed. It is now OK to drop
            // the returned `FutureState`s.
            let success: Vec<_> = future_state
                .into_iter()
                .map(|state| match state {
                    FutureState::Completed => true,
                    _ => false,
                })
                .collect();

            // Check which threads have been notified, excluding those which
            // future was cancelled.
            let notified: Vec<_> = wakers
                .iter()
                .enumerate()
                .map(|(i, test_waker)| {
                    // Count the notification unless the thread was cancelled
                    // since in that case the notification would be missed.
                    test_waker.take_notification() && i >= max_cancellations
                })
                .collect();

            // Count how many threads have either succeeded or have been
            // notified.
            let actual_aggregate_count =
                success
                    .iter()
                    .zip(notified.iter())
                    .fold(0, |count, (&success, &notified)| {
                        if success || notified {
                            count + 1
                        } else {
                            count
                        }
                    });

            // Compare with the number of event sinks that should eventually succeed.
            let min_expected_success_count = token_count.min(waiter_count - max_cancellations);
            if actual_aggregate_count < min_expected_success_count {
                panic!(
                    "Successful threads: {:?}; Notified threads: {:?}",
                    success, notified
                );
            }
        });
    }

    #[test]
    fn loom_event_two_consumers() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 0, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_spurious() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 1, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 1, 1, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_change_waker() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 0, 0, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_change_waker_spurious() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 1, 0, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_change_waker_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(1, 2, 0, 1, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_change_waker_spurious_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 4;
        loom_event_notify(2, 2, 1, 1, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_two_consumers_three_tokens() {
        const DEFAULT_PREEMPTION_BOUND: usize = 3;
        loom_event_notify(3, 2, 0, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 0, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_spurious() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 1, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(2, 3, 0, 1, false, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_change_waker() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 0, 0, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_change_waker_spurious() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 1, 0, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_change_waker_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 0, 1, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_change_waker_spurious_cancellation() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(3, 3, 1, 1, true, DEFAULT_PREEMPTION_BOUND);
    }
    #[test]
    fn loom_event_three_consumers_two_tokens() {
        const DEFAULT_PREEMPTION_BOUND: usize = 2;
        loom_event_notify(2, 3, 0, 0, false, DEFAULT_PREEMPTION_BOUND);
    }
}
