//! Trait for model input and replier ports.

use std::future::{ready, Future, Ready};

use crate::model::{Context, Model};

use super::markers;

/// A function, method or closures that can be used as an *input port*.
///
/// This trait is in particular implemented for any function or method with the
/// following signature, where it is implicitly assumed that the function
/// implements `Send + 'static`:
///
/// ```ignore
/// FnOnce(&mut M, T)
/// FnOnce(&mut M, T, &Context<M>)
/// async fn(&mut M, T)
/// async fn(&mut M, T, &Context<M>)
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
    fn call(self, model: &'a mut M, arg: T, context: &'a Context<M>) -> Self::Future;
}

impl<'a, M, F> InputFn<'a, M, (), markers::WithoutArguments> for F
where
    M: Model,
    F: FnOnce(&'a mut M) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, _arg: (), _context: &'a Context<M>) -> Self::Future {
        self(model);

        ready(())
    }
}

impl<'a, M, T, F> InputFn<'a, M, T, markers::WithoutContext> for F
where
    M: Model,
    F: FnOnce(&'a mut M, T) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, arg: T, _context: &'a Context<M>) -> Self::Future {
        self(model, arg);

        ready(())
    }
}

impl<'a, M, T, F> InputFn<'a, M, T, markers::WithContext> for F
where
    M: Model,
    F: FnOnce(&'a mut M, T, &'a Context<M>) + Send + 'static,
{
    type Future = Ready<()>;

    fn call(self, model: &'a mut M, arg: T, context: &'a Context<M>) -> Self::Future {
        self(model, arg, context);

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

    fn call(self, model: &'a mut M, _arg: (), _context: &'a Context<M>) -> Self::Future {
        self(model)
    }
}

impl<'a, M, T, Fut, F> InputFn<'a, M, T, markers::AsyncWithoutContext> for F
where
    M: Model,
    Fut: Future<Output = ()> + Send + 'a,
    F: FnOnce(&'a mut M, T) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, _context: &'a Context<M>) -> Self::Future {
        self(model, arg)
    }
}

impl<'a, M, T, Fut, F> InputFn<'a, M, T, markers::AsyncWithContext> for F
where
    M: Model,
    Fut: Future<Output = ()> + Send + 'a,
    F: FnOnce(&'a mut M, T, &'a Context<M>) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, context: &'a Context<M>) -> Self::Future {
        self(model, arg, context)
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
/// async fn(&mut M, T, &Context<M>) -> R
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
    fn call(self, model: &'a mut M, arg: T, context: &'a Context<M>) -> Self::Future;
}

impl<'a, M, R, Fut, F> ReplierFn<'a, M, (), R, markers::AsyncWithoutArguments> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, _arg: (), _context: &'a Context<M>) -> Self::Future {
        self(model)
    }
}

impl<'a, M, T, R, Fut, F> ReplierFn<'a, M, T, R, markers::AsyncWithoutContext> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M, T) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, _context: &'a Context<M>) -> Self::Future {
        self(model, arg)
    }
}

impl<'a, M, T, R, Fut, F> ReplierFn<'a, M, T, R, markers::AsyncWithContext> for F
where
    M: Model,
    Fut: Future<Output = R> + Send + 'a,
    F: FnOnce(&'a mut M, T, &'a Context<M>) -> Fut + Send + 'static,
{
    type Future = Fut;

    fn call(self, model: &'a mut M, arg: T, context: &'a Context<M>) -> Self::Future {
        self(model, arg, context)
    }
}
