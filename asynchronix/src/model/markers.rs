//! Marker types for simulation model methods.

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model, without any other argument.
#[derive(Debug)]
pub struct WithoutArguments {}

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model and a message, without scheduler argument.
#[derive(Debug)]
pub struct WithoutScheduler {}

/// Marker type for regular simulation model methods that take a mutable
/// reference to the model, a message and an explicit scheduler argument.
#[derive(Debug)]
pub struct WithScheduler {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model, without any other argument.
#[derive(Debug)]
pub struct AsyncWithoutArguments {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model and a message, without scheduler argument.
#[derive(Debug)]
pub struct AsyncWithoutScheduler {}

/// Marker type for asynchronous simulation model methods that take a mutable
/// reference to the model, a message and an explicit scheduler argument.
#[derive(Debug)]
pub struct AsyncWithScheduler {}
