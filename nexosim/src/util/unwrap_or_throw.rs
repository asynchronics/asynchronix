//! C++-style exceptions!

use std::any::Any;
use std::panic;

/// An extension trait that allows sending the error itself as a panic payload
/// when unwrapping fails.
pub(crate) trait UnwrapOrThrow<T> {
    fn unwrap_or_throw(self) -> T;
}

impl<T, E> UnwrapOrThrow<T> for Result<T, E>
where
    E: 'static + Any + Send,
{
    fn unwrap_or_throw(self) -> T {
        match self {
            Ok(v) => v,
            Err(e) => panic::panic_any(e),
        }
    }
}
