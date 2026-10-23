//! Wrapper for sensitive values (passwords, tickets, keys).
//!
//! See `docs/spec/logging.md`, section 2: secrets must never reach a log line.

use core::fmt;

use zeroize::Zeroize;

/// A sensitive value whose `Debug`/`Display` output is `<redacted>` and whose memory is
/// zeroed on drop.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    /// Wraps a sensitive value.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Gives access to the sensitive value. Never pass the result to a logging macro.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new(String::from("hunter2"));
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
    }
}
