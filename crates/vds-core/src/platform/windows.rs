//! Windows: there are no dmabufs, so no buffer handle can exist.

/// A GPU buffer handle; it cannot be constructed on Windows.
#[derive(Debug)]
pub enum BufferFd {}

impl BufferFd {
    /// Never called: no value of this type exists.
    pub fn raw(&self) -> i32 {
        match *self {}
    }
}
