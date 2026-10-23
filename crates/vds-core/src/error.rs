//! Error type of `vds-core`.

/// Errors returned by `vds-core`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A configuration value is invalid.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// An argument passed by the host is invalid.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The requested display head does not exist.
    #[error("unknown display head {0}")]
    UnknownHead(u32),
    /// The logging subsystem was already initialized.
    #[error("logging is already initialized")]
    LogAlreadyInitialized,
    /// An I/O error.
    #[error("i/o error")]
    Io(#[from] std::io::Error),
}

/// Result type of `vds-core`.
pub type Result<T> = std::result::Result<T, Error>;
