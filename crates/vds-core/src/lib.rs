//! Protocol-independent core of libvds.
//!
//! The host (QEMU, through `vds-capi`) feeds display content, cursor, audio and clipboard
//! data into a [`Core`]; protocol backends (`vds-rdp`) read from it and push client events
//! back, which the host drains on its main thread.

#![forbid(unsafe_code)]

pub mod auth;
pub mod codec;
pub mod config;
mod core;
pub mod cursor;
pub mod debug;
pub mod display;
pub mod error;
pub mod event;
pub mod geom;
pub mod log;
pub mod secret;
pub mod session;
pub mod signal;

pub use crate::core::Core;
pub use crate::error::{Error, Result};

#[doc(hidden)]
pub use tracing as __tracing;
