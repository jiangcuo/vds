//! Platform-specific types, behind one interface for both hosts.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::BufferFd;
#[cfg(windows)]
pub use windows::BufferFd;
