//! Trait for model input and replier ports.

use std::future::{ready, Future, Ready};

use crate::model::{markers, Model};
use crate::time::Scheduler;

/// A function, method or closures that can be used as an *input port*.
///
/// This trait is in particular implemented for any function or method with the
/// following signature, where it is implicitly assumed that the function
/// implements `Send + 'static`:
///
/// ```ignore
/// FnOnce(&mut M, T)
/// FnOnce(&mut M, T, &Scheduler<M>)
/// async fn(&mut M, T)
/// async fn(&mut M, T, &Scheduler<M>)
/// where
///     M: Model
/// ```
///
/// It is also implemented for the following signatures when `T=()`:
///
/// ```ignore
/// FnOnce(&mut M)
/// async fn(&mut M)
/// where
///     M: Model
/// ```
pub trait InputFn<'a, M: Model, T, S>: Send + 'static {
    /// The `Future` returned by the asynchronous method.
    type Future: Future<Output = ()> + Send + 'a;

    /// Calls the method.
    fn call(self, model: &'a mut M, arg: T, scheduler: &'a Scheduler<M>) -> Self::Future;
}

impl<'a, M, F> InputFn<'a, M, (), markers::WithoutArguments> for F
where
    M: Model,
    F: FnOnce(&'a mut M) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, _arg: (), _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model);

        ready(())
    }
}

impl<'a, M, T, F> InputFn<'a, M, T, markers::WithoutScheduler> for F
where
    M: Model,
    F: FnOnce(&'a mut M, T) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, arg: T, _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg);

        ready(())
    }
}

impl<'a, M, T, F> InputFn<'a, M, T, markers::WithScheduler> for F
where
    M: Model,
    F: FnOnce(&'a mut M, T, &'a Scheduler<M>) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, arg: T, scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg, scheduler);

        ready(())
    }
}

impl<'a, M, Fut, F> InputFn<'a, M, (), markers::AsyncWithoutArguments> for F
where
    M: Model,
    Fut: Future<Output = ()> + Send + 'a,
    F: FnOnce(&'a mut M) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, _arg: (), _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model)
    }
}

impl<'a, M, T, Fut, F> InputFn<'a, M, T, markers::AsyncWithoutScheduler> for F
where
    M: Model,
    Fut: Future<Output = ()> + Send + 'a,
    F: FnOnce(&'a mut M, T) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg)
    }
}

impl<'a, M, T, Fut, F> InputFn<'a, M, T, markers::AsyncWithScheduler> for F
where
    M: Model,
    Fut: Future<Output = ()> + Send + 'a,
    F: FnOnce(&'a mut M, T, &'a Scheduler<M>) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg, scheduler)
    }
}

/// A function, method or closure that can be used as a *replier port*.
///
/// This trait is in particular implemented for any function or method with the
/// following signature, where it is implicitly assumed that the function
/// implements `Send + 'static`:
///
/// ```ignore
/// async fn(&mut M, T) -> R
/// async fn(&mut M, T, &Scheduler<M>) -> R
/// where
///     M: Model
/// ```
///
/// It is also implemented for the following signatures when `T=()`:
///
/// ```ignore
/// async fn(&mut M) -> R
/// where
///     M: Model
/// ```
pub trait ReplierFn<'a, M: Model, T, R, S>: Send + 'static {
    /// The `Future` returned by the asynchronous method.
    type Future: Future<Output = R> + Send + 'a;

    /// Calls the method.
    fn call(self, model: &'a mut M, arg: T, scheduler: &'a Scheduler<M>) -> Self::Future;
}

impl<'a, M, R, Fut, F> ReplierFn<'a, M, (), R, markers::AsyncWithoutArguments> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, _arg: (), _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model)
    }
}

impl<'a, M, T, R, Fut, F> ReplierFn<'a, M, T, R, markers::AsyncWithoutScheduler> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M, T) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, _scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg)
    }
}

impl<'a, M, T, R, Fut, F> ReplierFn<'a, M, T, R, markers::AsyncWithScheduler> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M, T, &'a Scheduler<M>) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, scheduler: &'a Scheduler<M>) -> Self::Future {
        self(model, arg, scheduler)
    }
}
