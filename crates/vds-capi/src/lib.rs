//! C ABI of libvds. The generated header is `include/vds.h`; rules are in
//! `docs/spec/c-abi.md`.
//!
//! Thread annotations used in the documentation:
//! - `[any-thread]`: callable from any thread, never blocks on the network or the encoder;
//! - `[main-thread]`: host main thread only (QEMU: with the BQL held);
//! - `[callback]`: invoked by libvds only inside `vds_server_dispatch()`.

mod api;
mod types;

pub use api::*;
pub use types::*;

/// Major version of the ABI (incompatible changes).
pub const VDS_ABI_MAJOR: u32 = 2;
/// Minor version of the ABI (compatible additions).
pub const VDS_ABI_MINOR: u32 = 0;

/// Success.
pub const VDS_OK: i32 = 0;
/// An argument is invalid (NULL handle, bad size, unknown value).
pub const VDS_E_INVALID_ARG: i32 = -1;
/// The operation failed; see the error string when one is returned.
pub const VDS_E_FAILED: i32 = -2;
/// The operation is not allowed in the current state (e.g. a client is connected).
pub const VDS_E_STATE: i32 = -3;
/// Internal error (a bug in libvds; the event is logged).
pub const VDS_E_INTERNAL: i32 = -100;
