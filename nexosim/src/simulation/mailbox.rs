use std::fmt;

use crate::channel::{Receiver, Sender};
use crate::model::Model;

/// A model mailbox.
///
/// A mailbox is an entity associated to a model instance that collects all
/// messages sent to that model. The size of its internal buffer can be
/// optionally specified at construction time using
/// [`with_capacity()`](Mailbox::with_capacity).
pub struct Mailbox<M: Model>(pub(crate) Receiver<M>);

impl<M: Model> Mailbox<M> {
    /// Default capacity when created with `new` or `Default::default`.
    pub const DEFAULT_CAPACITY: usize = 16;

    /// Creates a new mailbox with capacity `Self::DEFAULT_CAPACITY`.
    pub fn new() -> Self {
        Self(Receiver::new(Self::DEFAULT_CAPACITY))
    }

    /// Creates a new mailbox with the specified capacity.
    ///
    /// # Panic
    ///
    /// The constructor will panic if the requested capacity is 0 or is greater
    /// than `usize::MAX/2 + 1`.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Receiver::new(capacity))
    }

    /// Returns a handle to this mailbox.
    pub fn address(&self) -> Address<M> {
        Address(self.0.sender())
    }
}

impl<M: Model> Default for Mailbox<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> fmt::Debug for Mailbox<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailbox")
            .field("mailbox_id", &self.0.channel_id().to_string())
            .finish_non_exhaustive()
    }
}

/// Handle to a model mailbox.
///
/// An address always points to the same mailbox. Unlike a [`Mailbox`], however,
/// an address can be cloned and shared between threads.
///
/// For the sake of convenience, methods that require an address by value will
/// typically also accept an `&Address` or an `&Mailbox` since these references
/// implement the `Into<Address>` trait, automatically invoking
/// `Address::clone()` or `Mailbox::address()` as appropriate.
pub struct Address<M: Model>(pub(crate) Sender<M>);

impl<M: Model> Clone for Address<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M: Model> From<&Address<M>> for Address<M> {
    /// Converts an [`Address`] reference into an [`Address`].
    ///
    /// This clones the reference and returns the clone.
    #[inline]
    fn from(s: &Address<M>) -> Address<M> {
        s.clone()
    }
}

impl<M: Model> From<&Mailbox<M>> for Address<M> {
    /// Converts a [Mailbox] reference into an [`Address`].
    ///
    /// This calls [`Mailbox::address()`] on the mailbox and returns the
    /// address.
    #[inline]
    fn from(s: &Mailbox<M>) -> Address<M> {
        s.address()
    }
}

impl<M: Model> fmt::Debug for Address<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Address")
            .field("mailbox_id", &self.0.channel_id().to_string())
            .finish_non_exhaustive()
    }
}
