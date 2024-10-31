use std::ops::Deref;

use asynchronix::ports::Output;

/// Observability trait.
pub trait Observable<T> {
    /// Observe the value.
    fn observe(&self) -> T;
}

impl<T> Observable<T> for T
where
    T: Clone,
{
    fn observe(&self) -> Self {
        self.clone()
    }
}

/// Observable state.
///
/// This object encapsulates state. Every state change is propagated to the
/// output.
#[derive(Debug)]
pub struct ObservableState<S, T>
where
    S: Observable<T> + Default,
    T: Clone + Send + 'static,
{
    /// State.
    state: S,

    /// Output used for observation.
    out: Output<T>,
}

impl<S, T> ObservableState<S, T>
where
    S: Observable<T> + Default,
    T: Clone + Send + 'static,
{
    /// New default state.
    pub fn new(out: Output<T>) -> Self {
        Self {
            state: S::default(),
            out,
        }
    }

    /// Get state.
    pub fn get(&self) -> &S {
        &self.state
    }

    /// Set state.
    pub async fn set(&mut self, value: S) {
        self.state = value;
        self.out.send(self.state.observe()).await;
    }

    /// Modify state using mutable reference.
    pub async fn modify<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut S) -> R,
    {
        let r = f(&mut self.state);
        self.out.send(self.state.observe()).await;
        r
    }

    /// Propagate value.
    pub async fn propagate(&mut self) {
        self.out.send(self.state.observe()).await;
    }
}

impl<S, T> Deref for ObservableState<S, T>
where
    S: Observable<T> + Default,
    T: Clone + Send + 'static,
{
    type Target = S;

    fn deref(&self) -> &S {
        &self.state
    }
}

/// Observable value.
pub type ObservableValue<T> = ObservableState<T, T>;
