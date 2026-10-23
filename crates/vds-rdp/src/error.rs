//! Error type of `vds-rdp`.

use std::net::SocketAddr;

/// Errors returned by `vds-rdp`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The TLS certificate or key is unusable.
    #[error("invalid TLS identity: {0}")]
    Tls(String),
    /// Binding a listening socket failed.
    #[error("failed to listen on {addr}")]
    Listen {
        /// Requested address.
        addr: SocketAddr,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
    /// The RDP thread is not running any more.
    #[error("the RDP server is stopped")]
    Stopped,
    /// An I/O error.
    #[error("i/o error")]
    Io(#[from] std::io::Error),
    /// A core error.
    #[error(transparent)]
    Core(#[from] vds_core::Error),
}

/// Result type of `vds-rdp`.
pub type Result<T> = std::result::Result<T, Error>;
