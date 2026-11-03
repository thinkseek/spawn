use thiserror::Error;

/// Errors raised by domain validation and invariant enforcement.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ValidationError {
    /// A label was empty or longer than the allowed byte length.
    #[error("invalid label length: {0}")]
    InvalidLength(usize),

    /// A label contained an ASCII control character.
    #[error("label contains a control character")]
    ControlCharacter,

    /// A numeric value fell outside its declared closed interval.
    #[error("value {value} is outside [{minimum}, {maximum}]")]
    OutOfRange {
        /// The rejected value.
        value: f64,
        /// Inclusive lower bound.
        minimum: f64,
        /// Inclusive upper bound.
        maximum: f64,
    },
}

/// Crate-level error surface shared by port contracts.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// A domain value failed validation.
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// A precondition declared by a port contract did not hold.
    #[error("invariant violated: {0}")]
    Invariant(String),
}

impl SpawnError {
    /// Build an invariant error with a fixed message.
    #[must_use]
    pub fn invariant(message: impl Into<String>) -> Self {
        Self::Invariant(message.into())
    }
}
