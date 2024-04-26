//! Marker types for simulation model methods.

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model, without any other argument.
#[derive(Debug)]
pub struct WithoutArguments {}

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model and a message, without context argument.
#[derive(Debug)]
pub struct WithoutContext {}

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model, a message and an explicit context argument.
#[derive(Debug)]
pub struct WithContext {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model, without any other argument.
#[derive(Debug)]
pub struct AsyncWithoutArguments {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model and a message, without context argument.
#[derive(Debug)]
pub struct AsyncWithoutContext {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model, a message and an explicit context argument.
#[derive(Debug)]
pub struct AsyncWithContext {}
