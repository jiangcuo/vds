//! Plain C types of the ABI.

use core::ffi::{c_char, c_void};

/// `VdsConfig::auth`: no password check.
pub const VDS_AUTH_NONE: u32 = 0;
/// `VdsConfig::auth`: TLS, password from the Client Info PDU.
pub const VDS_AUTH_TLS: u32 = 1;
/// `VdsConfig::auth`: TLS + CredSSP/NLA (default).
pub const VDS_AUTH_NLA: u32 = 2;

/// Codec bit: EGFX AVC420 (openh264 or VA-API).
pub const VDS_CODEC_AVC420: u32 = 1;
/// Codec bit: RemoteFX.
pub const VDS_CODEC_REMOTEFX: u32 = 2;
/// Codec bit: plain bitmaps.
pub const VDS_CODEC_BITMAP: u32 = 4;

/// Connected clients are kept when the password changes.
pub const VDS_CONNECTED_KEEP: u32 = 0;
/// The password change fails if a client is connected.
pub const VDS_CONNECTED_FAIL: u32 = 1;
/// Connected clients are disconnected when the password changes.
pub const VDS_CONNECTED_DISCONNECT: u32 = 2;

/// Password never expires (`vds_set_password_expiry`).
pub const VDS_EXPIRE_NEVER: i64 = 0x7FFF_FFFF_FFFF_FFFF;

/// Log format: one text line per event.
pub const VDS_LOG_FORMAT_TEXT: u32 = 0;
/// Log format: one JSON object per line.
pub const VDS_LOG_FORMAT_JSON: u32 = 1;
/// Log format: `VDS_LOG_FORMAT` environment variable, text by default.
pub const VDS_LOG_FORMAT_DEFAULT: u32 = 2;

/// Mouse button: left.
pub const VDS_BUTTON_LEFT: u32 = 0;
/// Mouse button: right.
pub const VDS_BUTTON_RIGHT: u32 = 1;
/// Mouse button: middle.
pub const VDS_BUTTON_MIDDLE: u32 = 2;
/// Mouse button: side (back).
pub const VDS_BUTTON_SIDE: u32 = 3;
/// Mouse button: extra (forward).
pub const VDS_BUTTON_EXTRA: u32 = 4;

/// Clipboard type: UTF-8 text.
pub const VDS_CLIPBOARD_TEXT: u32 = 1;
/// Clipboard type: PNG image.
pub const VDS_CLIPBOARD_PNG: u32 = 2;

/// Client state: TCP connection accepted.
pub const VDS_CLIENT_CONNECTED: u32 = 0;
/// Client state: authenticated and ready.
pub const VDS_CLIENT_INITIALIZED: u32 = 1;
/// Client state: disconnected.
pub const VDS_CLIENT_DISCONNECTED: u32 = 2;

/// Unknown value of `VdsClientInfo::rtt_ms`.
pub const VDS_RTT_UNKNOWN: u32 = 0xFFFF_FFFF;

/// `VdsConfig::debug_flags`: write `input.csv` (keys redacted).
pub const VDS_DEBUG_INPUT: u32 = 1;
/// `VdsConfig::debug_flags`: write `video-head<N>.h264`.
pub const VDS_DEBUG_VIDEO: u32 = 2;
/// `VdsConfig::debug_flags`: write `client-pdus.bin` (reserved, not implemented).
pub const VDS_DEBUG_PDU: u32 = 4;

/// `VdsConfig::sas_hotkey`: the client's Ctrl+Alt+End is injected as Ctrl+Alt+Del (default).
pub const VDS_SAS_CTRL_ALT_END: u32 = 0;
/// `VdsConfig::sas_hotkey`: the keys are injected unchanged.
pub const VDS_SAS_OFF: u32 = 1;

/// Server configuration. Initialize with `vds_config_init()`, then change fields.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VdsConfig {
    /// `sizeof(VdsConfig)`, set by `vds_config_init()`.
    pub struct_size: u32,
    /// `VDS_AUTH_*`.
    pub auth: u32,
    /// Allowed codecs, `VDS_CODEC_*` bits (order is fixed: avc420, remotefx, bitmap).
    pub codecs: u32,
    /// Frame rate cap (1..=120).
    pub max_fps: u32,
    /// Maximum video bitrate in kbit/s; 0 = automatic.
    pub bitrate_kbps: u32,
    /// Encoder threads; 0 = default (2).
    pub encoder_threads: u32,
    /// Refinement delay of static lossy areas in ms; 0 disables.
    pub refine_delay_ms: u32,
    /// How long to wait for the client's EGFX channel, in ms.
    pub egfx_grace_ms: u32,
    /// Enable audio playback.
    pub audio: bool,
    /// Enable clipboard sharing.
    pub clipboard: bool,
    /// Ask the guest to adopt the client resolution on connection.
    pub resize_on_connect: bool,
    /// VM name for logs; may be NULL. Copied by `vds_server_new()`.
    pub vm_name: *const c_char,
    /// Directory receiving per-session diagnostic files; NULL disables debugging.
    /// Copied by `vds_server_new()`.
    pub debug_dir: *const c_char,
    /// `VDS_DEBUG_*` bits, applied when `debug_dir` is set.
    pub debug_flags: u32,
    /// `VDS_SAS_*`: what the client's Ctrl+Alt+End turns into for the guest.
    pub sas_hotkey: u32,
    /// Highest quantizer the encoder may use (1..=51); 0 = default (30).
    ///
    /// When the link or the decoder cannot keep up, libvds prefers a lower
    /// frame rate over a blurry picture, so VBV is not allowed to answer a bitrate it cannot
    /// pay for by quantizing the frame into a blur. 51 is H.264's own limit.
    pub qp_max: u32,
    /// Quantizer of a refinement frame (1..=51); 0 = default (10). Must not exceed `qp_max`.
    pub refine_qp: u32,
    /// Send the still part of a head as lossless ClearCodec tiles beside its H.264 frame;
    /// this replaces the refinement frame of `refine_delay_ms`.
    pub egfx_mixed: bool,
    /// H.264 encoder: "openh264" or "vaapi"; NULL = "openh264". "vaapi" falls back to openh264 when the
    /// build or the host lacks it. Copied by `vds_server_new()`. Since ABI 1.6.
    pub encoder: *const c_char,
    /// DRM render node of the VA-API encoder (e.g. "/dev/dri/renderD128"); NULL lets FFmpeg
    /// pick one. Copied by `vds_server_new()`. Since ABI 1.6.
    pub render_node: *const c_char,
}

/// Logging configuration for `vds_log_init()`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VdsLogConfig {
    /// `sizeof(VdsLogConfig)`.
    pub struct_size: u32,
    /// `EnvFilter` directive (e.g. "info,vds_rdp=debug"); NULL = `VDS_LOG` or "info".
    pub filter: *const c_char,
    /// `VDS_LOG_FORMAT_*`.
    pub format: u32,
}

/// Gives a host buffer back to the host (`vds_head_attach_shared()`). `[any-thread callback]`
pub type VdsReleaseFn = Option<unsafe extern "C" fn(opaque: *mut c_void)>;

/// A rectangle in pixels.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VdsRect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// One monitor of a client-requested layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VdsMonitor {
    /// Left edge on the client desktop.
    pub left: i32,
    /// Top edge on the client desktop.
    pub top: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Physical width in mm (0 = unknown).
    pub width_mm: u32,
    /// Physical height in mm (0 = unknown).
    pub height_mm: u32,
    /// Primary monitor.
    pub primary: bool,
}

/// Information about a client. Strings are NUL-terminated, truncated if needed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VdsClientInfo {
    /// Session id.
    pub sid: u64,
    /// Remote address ("ip:port"), empty if unknown.
    pub peer: [c_char; 64],
    /// Authenticated user, empty if none.
    pub user: [c_char; 128],
    /// Codec in use ("avc420", "legacy"), empty before negotiation.
    pub codec: [c_char; 16],
    /// Connection time, seconds since the Unix epoch.
    pub connected_at: i64,
    /// Measured frames per second.
    pub fps: u32,
    /// Measured video bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// Round-trip time in ms, `VDS_RTT_UNKNOWN` if not measured.
    pub rtt_ms: u32,
}

/// Callbacks from libvds to the host. NULL members are ignored.
///
/// `wakeup` is `[any-thread]`; every other member is `[callback]` (called only inside
/// `vds_server_dispatch()`). Callbacks may call `[any-thread]` functions but must not call
/// `vds_server_dispatch()` or `vds_server_free()`. Pointer arguments are valid only during
/// the call.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VdsCallbacks {
    /// Events are pending: schedule `vds_server_dispatch()` on the main thread.
    pub wakeup: Option<unsafe extern "C" fn(opaque: *mut c_void)>,
    /// Key press/release, XT scancode set 1 (`extended` = 0xE0 prefix).
    pub key: Option<unsafe extern "C" fn(opaque: *mut c_void, scancode: u8, extended: bool, down: bool)>,
    /// Unicode key event (UTF-16 code unit).
    pub unicode: Option<unsafe extern "C" fn(opaque: *mut c_void, code: u16, down: bool)>,
    /// Client lock-key state.
    pub lock_sync: Option<unsafe extern "C" fn(opaque: *mut c_void, scroll: bool, num: bool, caps: bool)>,
    /// Absolute pointer position within `head` (`width`x`height`).
    pub pointer_abs:
        Option<unsafe extern "C" fn(opaque: *mut c_void, head: u32, x: u32, y: u32, width: u32, height: u32)>,
    /// Relative pointer motion.
    pub pointer_rel: Option<unsafe extern "C" fn(opaque: *mut c_void, dx: i32, dy: i32)>,
    /// Mouse button (`VDS_BUTTON_*`).
    pub button: Option<unsafe extern "C" fn(opaque: *mut c_void, button: u32, down: bool)>,
    /// Wheel notches; positive `dy` = up, positive `dx` = right.
    pub wheel: Option<unsafe extern "C" fn(opaque: *mut c_void, dx: i32, dy: i32)>,
    /// Client requested a monitor layout.
    pub layout: Option<unsafe extern "C" fn(opaque: *mut c_void, monitors: *const VdsMonitor, count: u32)>,
    /// The client copied data of these `VDS_CLIPBOARD_*` types.
    pub clipboard_grab: Option<unsafe extern "C" fn(opaque: *mut c_void, types: *const u32, count: u32)>,
    /// The client clipboard is empty.
    pub clipboard_release: Option<unsafe extern "C" fn(opaque: *mut c_void)>,
    /// The client wants the host clipboard; answer with `vds_clipboard_data()`.
    pub clipboard_request: Option<unsafe extern "C" fn(opaque: *mut c_void, ty: u32)>,
    /// Client clipboard content requested with `vds_clipboard_request()`.
    pub clipboard_data: Option<unsafe extern "C" fn(opaque: *mut c_void, ty: u32, data: *const u8, len: usize)>,
    /// Client lifecycle (`VDS_CLIENT_*`); `reason` is non-NULL only for disconnections.
    pub client_state: Option<
        unsafe extern "C" fn(opaque: *mut c_void, info: *const VdsClientInfo, state: u32, reason: *const c_char),
    >,
}
