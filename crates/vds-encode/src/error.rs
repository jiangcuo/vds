//! Error type of `vds-encode`.

/// Errors returned by `vds-encode`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// libvds was built without this encoder (VA-API without the `vaapi` feature).
    #[error("this encoder is not available in this build")]
    Unavailable,
    /// The encoder could not be opened with the requested parameters.
    #[error("failed to open the encoder: {0}")]
    Open(String),
    /// Colour conversion failed.
    #[error("colour conversion failed: {0}")]
    Convert(String),
    /// The encoder failed to encode a picture.
    #[error("the encoder failed to encode a picture")]
    Encode,
    /// The encoder refused a rate-control change.
    #[error("the encoder refused the rate-control change")]
    Reconfig,
    /// Picture dimensions are invalid.
    #[error("invalid picture size {0}x{1}")]
    InvalidSize(u32, u32),
    /// A picture does not match the encoder dimensions.
    #[error("picture is {got_w}x{got_h}, encoder expects {want_w}x{want_h}")]
    SizeMismatch {
        /// Picture width.
        got_w: u32,
        /// Picture height.
        got_h: u32,
        /// Encoder width.
        want_w: u32,
        /// Encoder height.
        want_h: u32,
    },
}

/// Result type of `vds-encode`.
pub type Result<T> = std::result::Result<T, Error>;
