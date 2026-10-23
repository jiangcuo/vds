//! `extern "C"` entry points. Every function body runs inside `ffi_guard!`, so a panic never
//! crosses the ABI (c-abi.md, section 7).

use core::ffi::{c_char, c_void};
use std::ffi::{CStr, CString};
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing::{error, warn};
use vds_core::Core;
use vds_core::auth::{AuthMode, ConnectedAction, Credentials};
use vds_core::codec::CodecSet;
use vds_core::config::{SasHotkey, ServerConfig, VideoEncoderKind};
use vds_core::cursor::CursorImage;
use vds_core::display::Head;
use vds_core::event::{ClientEvent, ClientInfo, ClientState, ClipboardType, MouseButton};
use vds_core::geom::Rect;
use vds_core::log::{LogConfig, LogFormat};
use vds_core::secret::Secret;
use vds_rdp::{RdpServer, TlsIdentity};

use crate::types::*;
use crate::{VDS_ABI_MAJOR, VDS_ABI_MINOR, VDS_E_FAILED, VDS_E_INTERNAL, VDS_E_INVALID_ARG, VDS_E_STATE, VDS_OK};

macro_rules! ffi_guard {
    ($default:expr, $body:block) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(v) => v,
            Err(_) => {
                error!("panic in a libvds C ABI function");
                $default
            }
        }
    };
}

/// Host-provided context pointer.
struct Opaque(*mut c_void);

// SAFETY: the pointer is never dereferenced by libvds; it is only passed back to the host's
// callbacks. `wakeup` is documented as callable from any thread, every other callback runs on
// the thread calling `vds_server_dispatch()`.
unsafe impl Send for Opaque {}
// SAFETY: see above; libvds never accesses the pointee.
unsafe impl Sync for Opaque {}

/// A libvds server instance (opaque).
pub struct VdsServer {
    core: Core,
    callbacks: VdsCallbacks,
    opaque: Arc<Opaque>,
    tls: Mutex<Option<TlsIdentity>>,
    rdp: Mutex<Option<RdpServer>>,
}

/// A display head (opaque).
pub struct VdsHead {
    core: Core,
    head: Arc<Head>,
}

/// Writes `msg` to `*err` if `err` is not NULL.
///
/// # Safety
/// `err` must be NULL or point to writable storage for one pointer.
unsafe fn set_err(err: *mut *mut c_char, msg: impl core::fmt::Display) {
    if err.is_null() {
        return;
    }
    let s = msg.to_string().replace('\0', " ");
    let c = CString::new(s).unwrap_or_default();
    // SAFETY: `err` is non-NULL and writable per the function contract.
    unsafe { *err = c.into_raw() };
}

/// Reads an optional UTF-8 C string.
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn opt_str(p: *const c_char) -> Result<Option<String>, String> {
    if p.is_null() {
        return Ok(None);
    }
    // SAFETY: non-NULL and NUL-terminated per the function contract.
    let s = unsafe { CStr::from_ptr(p) };
    s.to_str()
        .map(|s| Some(s.to_owned()))
        .map_err(|_| String::from("string is not valid UTF-8"))
}

/// # Safety
/// `p` must be NULL or a pointer returned by `vds_server_new()` and not yet freed.
unsafe fn as_server<'a>(p: *const VdsServer) -> Option<&'a VdsServer> {
    // SAFETY: per the function contract.
    unsafe { p.as_ref() }
}

/// # Safety
/// `p` must be NULL or a pointer returned by `vds_head_add()` and not yet removed.
unsafe fn as_head<'a>(p: *const VdsHead) -> Option<&'a VdsHead> {
    // SAFETY: per the function contract.
    unsafe { p.as_ref() }
}

/// Builds a slice from a pointer and a length; NULL is accepted only for `len == 0`.
///
/// # Safety
/// `p` must point to `len` readable elements when `len > 0`.
unsafe fn slice<'a, T>(p: *const T, len: usize) -> Option<&'a [T]> {
    if len == 0 {
        return Some(&[]);
    }
    if p.is_null() {
        return None;
    }
    // SAFETY: non-NULL and valid for `len` elements per the function contract.
    Some(unsafe { core::slice::from_raw_parts(p, len) })
}

fn copy_str(dst: &mut [c_char], s: &str) {
    let max = dst.len().saturating_sub(1);
    let bytes = s.as_bytes();
    // Truncate on a UTF-8 character boundary.
    let mut n = bytes.len().min(max);
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    for (d, b) in dst.iter_mut().zip(&bytes[..n]) {
        *d = c_char::from_ne_bytes([*b]);
    }
    if let Some(d) = dst.get_mut(n) {
        *d = 0;
    }
}

fn client_info(info: &ClientInfo) -> VdsClientInfo {
    let mut out = VdsClientInfo {
        sid: info.sid,
        peer: [0; 64],
        user: [0; 128],
        codec: [0; 16],
        connected_at: info
            .connected_at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        fps: info.fps,
        bitrate_kbps: info.bitrate_kbps,
        rtt_ms: info.rtt_ms.unwrap_or(VDS_RTT_UNKNOWN),
    };
    copy_str(&mut out.peer, &info.peer.map(|p| p.to_string()).unwrap_or_default());
    copy_str(&mut out.user, info.user.as_deref().unwrap_or(""));
    copy_str(&mut out.codec, info.codec.unwrap_or(""));
    out
}

impl VdsServer {
    fn ensure_started(&self) -> Result<(), String> {
        let mut rdp = self.rdp.lock();
        if rdp.is_some() {
            return Ok(());
        }
        let tls = match self.tls.lock().clone() {
            Some(t) => t,
            None => {
                warn!("no TLS certificate configured, generating a self-signed one");
                TlsIdentity::self_signed("localhost").map_err(|e| e.to_string())?
            }
        };
        *rdp = Some(RdpServer::start(self.core.clone(), tls).map_err(|e| e.to_string())?);
        Ok(())
    }

    fn dispatch(&self) {
        let cb = &self.callbacks;
        let op = self.opaque.0;
        self.core.drain_events(|ev| {
            match ev {
                ClientEvent::Key {
                    scancode,
                    extended,
                    down,
                } => {
                    if let Some(f) = cb.key {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, scancode, extended, down) }
                    }
                }
                ClientEvent::Unicode { code, down } => {
                    if let Some(f) = cb.unicode {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, code, down) }
                    }
                }
                ClientEvent::LockSync { scroll, num, caps } => {
                    if let Some(f) = cb.lock_sync {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, scroll, num, caps) }
                    }
                }
                ClientEvent::PointerAbs {
                    head,
                    x,
                    y,
                    width,
                    height,
                } => {
                    if let Some(f) = cb.pointer_abs {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, head, x, y, width, height) }
                    }
                }
                ClientEvent::PointerRel { dx, dy } => {
                    if let Some(f) = cb.pointer_rel {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, dx, dy) }
                    }
                }
                ClientEvent::Button { button, down } => {
                    let b = match button {
                        MouseButton::Left => VDS_BUTTON_LEFT,
                        MouseButton::Right => VDS_BUTTON_RIGHT,
                        MouseButton::Middle => VDS_BUTTON_MIDDLE,
                        MouseButton::Side => VDS_BUTTON_SIDE,
                        MouseButton::Extra => VDS_BUTTON_EXTRA,
                    };
                    if let Some(f) = cb.button {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, b, down) }
                    }
                }
                ClientEvent::Wheel { dx, dy } => {
                    if let Some(f) = cb.wheel {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, dx, dy) }
                    }
                }
                ClientEvent::Layout(monitors) => {
                    let mons: Vec<VdsMonitor> = monitors
                        .iter()
                        .map(|m| VdsMonitor {
                            left: m.left,
                            top: m.top,
                            width: m.width,
                            height: m.height,
                            width_mm: m.width_mm,
                            height_mm: m.height_mm,
                            primary: m.primary,
                        })
                        .collect();
                    if let (Some(f), Ok(n)) = (cb.layout, u32::try_from(mons.len())) {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, mons.as_ptr(), n) }
                    }
                }
                ClientEvent::ClipboardGrab(types) => {
                    let raw: Vec<u32> = types.iter().map(|t| t.to_raw()).collect();
                    if let (Some(f), Ok(n)) = (cb.clipboard_grab, u32::try_from(raw.len())) {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, raw.as_ptr(), n) }
                    }
                }
                ClientEvent::ClipboardRelease => {
                    if let Some(f) = cb.clipboard_release {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op) }
                    }
                }
                ClientEvent::ClipboardRequest(ty) => {
                    if let Some(f) = cb.clipboard_request {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, ty.to_raw()) }
                    }
                }
                ClientEvent::ClipboardData(ty, data) => {
                    if let Some(f) = cb.clipboard_data {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, ty.to_raw(), data.as_ptr(), data.len()) }
                    }
                }
                ClientEvent::Client(info, state) => {
                    let raw = client_info(&info);
                    let st = match state {
                        ClientState::Connected => VDS_CLIENT_CONNECTED,
                        ClientState::Initialized => VDS_CLIENT_INITIALIZED,
                        ClientState::Disconnected => VDS_CLIENT_DISCONNECTED,
                    };
                    let reason = info
                        .reason
                        .as_deref()
                        .map(|r| CString::new(r.replace('\0', " ")).unwrap_or_default());
                    let reason_ptr = reason.as_ref().map_or(core::ptr::null(), |r| r.as_ptr());
                    if let Some(f) = cb.client_state {
                        // SAFETY: host-provided callback and context; arguments live for the call.
                        unsafe { f(op, &raw, st, reason_ptr) }
                    }
                }
            }
        });
    }
}

/// Returns `VDS_ABI_MAJOR << 16 | VDS_ABI_MINOR`. `[any-thread]`
#[unsafe(no_mangle)]
pub extern "C" fn vds_abi_version() -> u32 {
    (VDS_ABI_MAJOR << 16) | VDS_ABI_MINOR
}

/// Fills `cfg` with the default configuration. `[any-thread]`
///
/// # Safety
/// `cfg` must be NULL or point to writable memory for a `VdsConfig`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_config_init(cfg: *mut VdsConfig) {
    ffi_guard!((), {
        let d = ServerConfig::default();
        let c = VdsConfig {
            struct_size: u32::try_from(size_of::<VdsConfig>()).unwrap_or(0),
            auth: VDS_AUTH_NLA,
            codecs: u32::from(d.codecs.bits()),
            max_fps: d.max_fps,
            bitrate_kbps: d.bitrate_kbps,
            encoder_threads: d.encoder_threads,
            refine_delay_ms: d.refine_delay_ms,
            egfx_grace_ms: d.egfx_grace_ms,
            audio: d.audio,
            clipboard: d.clipboard,
            resize_on_connect: d.resize_on_connect,
            vm_name: core::ptr::null(),
            debug_dir: core::ptr::null(),
            debug_flags: 0,
            sas_hotkey: VDS_SAS_CTRL_ALT_END,
            qp_max: u32::from(d.qp_max),
            refine_qp: u32::from(d.refine_qp),
            egfx_mixed: d.egfx_mixed,
            encoder: core::ptr::null(),
            render_node: core::ptr::null(),
        };
        // SAFETY: `cfg` is NULL or writable per the function contract.
        if let Some(out) = unsafe { cfg.as_mut() } {
            *out = c;
        }
    })
}

/// Installs the process-wide log subscriber (stderr). Call at most once. `[main-thread]`
///
/// Returns `VDS_OK`, or an error code with a message in `*err` (free with
/// `vds_free_string()`).
///
/// # Safety
/// `cfg` must be NULL or point to a valid `VdsLogConfig`; `err` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_log_init(cfg: *const VdsLogConfig, err: *mut *mut c_char) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        let mut lc = LogConfig::default();
        // SAFETY: per the function contract.
        if let Some(c) = unsafe { cfg.as_ref() } {
            // SAFETY: `filter` is NULL or NUL-terminated per the struct documentation.
            match unsafe { opt_str(c.filter) } {
                Ok(f) => lc.filter = f,
                Err(e) => {
                    // SAFETY: `err` per the function contract.
                    unsafe { set_err(err, e) };
                    return VDS_E_INVALID_ARG;
                }
            }
            lc.format = match c.format {
                VDS_LOG_FORMAT_TEXT => Some(LogFormat::Text),
                VDS_LOG_FORMAT_JSON => Some(LogFormat::Json),
                _ => None,
            };
        }
        match vds_core::log::init(&lc) {
            Ok(()) => VDS_OK,
            Err(e) => {
                // SAFETY: `err` per the function contract.
                unsafe { set_err(err, e) };
                VDS_E_FAILED
            }
        }
    })
}

/// Creates a server. No socket is opened yet. `[main-thread]`
///
/// Returns NULL on error, with a message in `*err`.
///
/// # Safety
/// `cfg` must point to a `VdsConfig` initialized with `vds_config_init()`; `cb` must be NULL
/// or point to a `VdsCallbacks` (copied); `err` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_new(
    cfg: *const VdsConfig,
    cb: *const VdsCallbacks,
    opaque: *mut c_void,
    err: *mut *mut c_char,
) -> *mut VdsServer {
    ffi_guard!(core::ptr::null_mut(), {
        // SAFETY: per the function contract.
        let Some(c) = (unsafe { cfg.as_ref() }) else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "config is NULL") };
            return core::ptr::null_mut();
        };
        let result = (|| -> Result<VdsServer, String> {
            if (c.struct_size as usize) < size_of::<VdsConfig>() {
                return Err(String::from("VdsConfig not initialized with vds_config_init()"));
            }
            let auth = match c.auth {
                VDS_AUTH_NONE => AuthMode::None,
                VDS_AUTH_TLS => AuthMode::Tls,
                VDS_AUTH_NLA => AuthMode::Nla,
                other => return Err(format!("unknown auth mode {other}")),
            };
            let sas_hotkey = match c.sas_hotkey {
                VDS_SAS_CTRL_ALT_END => SasHotkey::CtrlAltEnd,
                VDS_SAS_OFF => SasHotkey::Off,
                other => return Err(format!("unknown sas-hotkey {other}")),
            };
            let d = ServerConfig::default();
            // 0 selects the default, as `encoder_threads` does; anything else must be a
            // quantizer H.264 can represent.
            let qp_max = if c.qp_max == 0 {
                d.qp_max
            } else {
                u8::try_from(c.qp_max)
                    .ok()
                    .filter(|q| (1..=51).contains(q))
                    .ok_or_else(|| format!("qp-max {} out of range 1..=51", c.qp_max))?
            };
            let refine_qp = if c.refine_qp == 0 {
                d.refine_qp
            } else {
                u8::try_from(c.refine_qp)
                    .ok()
                    .filter(|q| (1..=51).contains(q))
                    .ok_or_else(|| format!("refine-qp {} out of range 1..=51", c.refine_qp))?
            };
            // SAFETY: the strings are NULL or NUL-terminated per the struct documentation.
            // SAFETY: as above.
            let vm_name = unsafe { opt_str(c.vm_name) }?;
            // SAFETY: as above.
            let debug_dir = unsafe { opt_str(c.debug_dir) }?;
            // SAFETY: as above.
            let encoder = match unsafe { opt_str(c.encoder) }? {
                Some(name) => VideoEncoderKind::parse(&name).map_err(|e| e.to_string())?,
                None => d.encoder,
            };
            // SAFETY: as above.
            let render_node = unsafe { opt_str(c.render_node) }?;
            let config = ServerConfig {
                auth,
                codecs: CodecSet::from_bits(c.codecs).map_err(|e| e.to_string())?,
                max_fps: c.max_fps,
                bitrate_kbps: c.bitrate_kbps,
                encoder_threads: c.encoder_threads,
                encoder,
                render_node,
                qp_max,
                refine_qp,
                refine_delay_ms: c.refine_delay_ms,
                egfx_mixed: c.egfx_mixed,
                egfx_grace_ms: c.egfx_grace_ms,
                audio: c.audio,
                clipboard: c.clipboard,
                resize_on_connect: c.resize_on_connect,
                sas_hotkey,
                vm_name,
                debug_dir,
                debug_flags: vds_core::debug::DebugFlags::from_bits(c.debug_flags),
            };
            let core = Core::new(config).map_err(|e| e.to_string())?;
            // SAFETY: `cb` is NULL or valid per the function contract.
            let callbacks = unsafe { cb.as_ref() }.copied().unwrap_or_default();
            let opaque = Arc::new(Opaque(opaque));
            if let Some(wakeup) = callbacks.wakeup {
                let op = Arc::clone(&opaque);
                core.set_waker(move || {
                    // SAFETY: `wakeup` is documented as callable from any thread.
                    unsafe { wakeup(op.0) }
                });
            }
            Ok(VdsServer {
                core,
                callbacks,
                opaque,
                tls: Mutex::new(None),
                rdp: Mutex::new(None),
            })
        })();
        match result {
            Ok(s) => Box::into_raw(Box::new(s)),
            Err(e) => {
                // SAFETY: `err` per the function contract.
                unsafe { set_err(err, e) };
                core::ptr::null_mut()
            }
        }
    })
}

/// Sets the TLS certificate chain and private key (PEM). Applies to new connections.
/// Without it, a self-signed certificate is generated when the server starts. `[main-thread]`
///
/// # Safety
/// `server` must be a valid server; the PEM strings NUL-terminated; `err` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_set_tls(
    server: *mut VdsServer,
    cert_pem: *const c_char,
    key_pem: *const c_char,
    err: *mut *mut c_char,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let (Some(s), Ok(Some(cert)), Ok(Some(key))) =
            (unsafe { as_server(server) }, unsafe { opt_str(cert_pem) }, unsafe {
                opt_str(key_pem)
            })
        else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "invalid server, certificate or key") };
            return VDS_E_INVALID_ARG;
        };
        let tls = match TlsIdentity::from_pem(cert.as_bytes(), key.as_bytes()) {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: `err` per the function contract.
                unsafe { set_err(err, e) };
                return VDS_E_FAILED;
            }
        };
        if let Some(rdp) = s.rdp.lock().as_ref()
            && let Err(e) = rdp.set_tls(tls.clone())
        {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, e) };
            return VDS_E_FAILED;
        }
        *s.tls.lock() = Some(tls);
        VDS_OK
    })
}

/// Listens on `addr`:`port` (`addr` NULL = all IPv4 interfaces, port 0 = any free port).
/// The bound port is stored in `*bound_port` when not NULL. `[main-thread]`
///
/// # Safety
/// `server` must be valid; `addr` NULL or NUL-terminated; `bound_port` NULL or writable;
/// `err` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_listen(
    server: *mut VdsServer,
    addr: *const c_char,
    port: u16,
    bound_port: *mut u16,
    err: *mut *mut c_char,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let (Some(s), Ok(host)) = (unsafe { as_server(server) }, unsafe { opt_str(addr) }) else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "invalid server or address") };
            return VDS_E_INVALID_ARG;
        };
        let host = host.unwrap_or_else(|| String::from("0.0.0.0"));
        let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        let resolved: Option<SocketAddr> = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next());
        let Some(sa) = resolved else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, format!("cannot resolve `{host}`")) };
            return VDS_E_INVALID_ARG;
        };
        if let Err(e) = s.ensure_started() {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, e) };
            return VDS_E_FAILED;
        }
        let res = s.rdp.lock().as_ref().map(|r| r.listen(sa));
        match res {
            Some(Ok(bound)) => {
                // SAFETY: `bound_port` is NULL or writable per the function contract.
                if let Some(p) = unsafe { bound_port.as_mut() } {
                    *p = bound.port();
                }
                VDS_OK
            }
            Some(Err(e)) => {
                // SAFETY: `err` per the function contract.
                unsafe {
                    set_err(
                        err,
                        format!(
                            "{e}: {}",
                            std::error::Error::source(&e).map(|s| s.to_string()).unwrap_or_default()
                        ),
                    )
                };
                VDS_E_FAILED
            }
            None => VDS_E_STATE,
        }
    })
}

/// Serves an already connected TCP socket (fd on Unix, `SOCKET` on Windows). On `VDS_OK`
/// ownership has moved to libvds; on any error the caller still owns the socket. With
/// `skipauth` no password is required. `[main-thread]`
///
/// # Safety
/// `server` must be valid and `sock` an open, connected TCP socket not used elsewhere.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_add_client(server: *mut VdsServer, sock: isize, skipauth: bool) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return VDS_E_INVALID_ARG;
        };
        if sock < 0 {
            return VDS_E_INVALID_ARG;
        }
        // Ownership of the socket moves to libvds only after this point, so on the error
        // returns above the caller still owns (and closes) it.
        if s.ensure_started().is_err() {
            return VDS_E_FAILED;
        }
        // SAFETY: `sock` is an open, connected TCP socket owned by the caller and handed over.
        let stream = unsafe { tcp_stream_from_raw(sock) };
        let res = s.rdp.lock().as_ref().map(|r| r.add_client(stream, skipauth));
        if !matches!(res, Some(Ok(()))) {
            // The socket was consumed (and closed) anyway: report success to keep the
            // ownership contract simple, and log the failure.
            error!("RDP thread unavailable, client socket closed");
        }
        VDS_OK
    })
}

#[cfg(unix)]
unsafe fn tcp_stream_from_raw(sock: isize) -> std::net::TcpStream {
    use std::os::fd::FromRawFd as _;
    // SAFETY: the caller transfers ownership of an open TCP socket descriptor.
    unsafe { std::net::TcpStream::from_raw_fd(i32::try_from(sock).unwrap_or(-1)) }
}

#[cfg(windows)]
unsafe fn tcp_stream_from_raw(sock: isize) -> std::net::TcpStream {
    use std::os::windows::io::FromRawSocket as _;
    // SAFETY: the caller transfers ownership of an open TCP SOCKET.
    unsafe { std::net::TcpStream::from_raw_socket(sock.unsigned_abs() as u64) }
}

/// Delivers pending events through the callbacks. Call it when `wakeup` fired.
/// `[main-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_dispatch(server: *mut VdsServer) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let Some(s) = unsafe { as_server(server) } {
            s.dispatch();
        }
    })
}

/// Disconnects every client. `[any-thread]`
///
/// # Safety
/// `server` must be valid; `reason` NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_disconnect_all(server: *mut VdsServer, reason: *const c_char) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return VDS_E_INVALID_ARG;
        };
        // SAFETY: per the function contract.
        let reason = unsafe { opt_str(reason) }
            .ok()
            .flatten()
            .unwrap_or_else(|| String::from("disconnected by the host"));
        s.core.disconnect_all(&reason);
        VDS_OK
    })
}

/// Enables or disables the optional debug traces at runtime (`VDS_DEBUG_*` bits). `[any-thread]`
///
/// `session.log`, `client.json`, `frames.csv`, `pointer.csv` and `audio.csv` are written as
/// soon as a debug directory is configured; the flags only add `input.csv` and the raw H.264
/// stream. The directory itself cannot change after `vds_server_new()`.
///
/// Returns `VDS_OK`, `VDS_E_STATE` if no directory is configured, or an error code with a
/// message in `*err` (free with `vds_free_string()`).
///
/// # Safety
/// `server` must be NULL or a valid server; `err` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_set_debug(server: *mut VdsServer, flags: u32, err: *mut *mut c_char) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(_s) = (unsafe { as_server(server) }) else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "invalid server") };
            return VDS_E_INVALID_ARG;
        };
        if vds_core::debug::directory().is_none() {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "no debug directory: start the server with debug_dir set") };
            return VDS_E_STATE;
        }
        vds_core::debug::set_flags(vds_core::debug::DebugFlags::from_bits(flags));
        VDS_OK
    })
}

/// Stops the server, closes every connection and frees it. No callback is invoked after
/// this returns. `[main-thread]`
///
/// # Safety
/// `server` must be NULL or a server returned by `vds_server_new()`; it must not be used
/// afterwards. Every head must have been removed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_free(server: *mut VdsServer) {
    ffi_guard!((), {
        if server.is_null() {
            return;
        }
        // SAFETY: the pointer came from `Box::into_raw` in `vds_server_new()`.
        let s = unsafe { Box::from_raw(server) };
        if let Some(rdp) = s.rdp.lock().take() {
            rdp.shutdown();
        }
        drop(s);
    })
}

/// Frees a string returned by libvds. `[any-thread]`
///
/// # Safety
/// `s` must be NULL or a string returned through a `char **err` parameter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_free_string(s: *mut c_char) {
    ffi_guard!((), {
        if !s.is_null() {
            // SAFETY: the string came from `CString::into_raw` in `set_err`.
            drop(unsafe { CString::from_raw(s) });
        }
    })
}

/// Sets (or clears, with `password` NULL) the credentials clients must present
/// (QMP `set_password`). Resets the expiry to "never". `connected` is `VDS_CONNECTED_*`;
/// `one_time` invalidates the password after the first successful login. `[main-thread]`
///
/// # Safety
/// `server` must be valid; strings NULL or NUL-terminated; `err` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_set_password(
    server: *mut VdsServer,
    username: *const c_char,
    password: *const c_char,
    domain: *const c_char,
    connected: u32,
    one_time: bool,
    err: *mut *mut c_char,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return VDS_E_INVALID_ARG;
        };
        // SAFETY: per the function contract.
        let parsed = (unsafe { opt_str(username) }, unsafe { opt_str(password) }, unsafe {
            opt_str(domain)
        });
        let (Ok(user), Ok(pass), Ok(domain)) = parsed else {
            // SAFETY: `err` per the function contract.
            unsafe { set_err(err, "strings must be valid UTF-8") };
            return VDS_E_INVALID_ARG;
        };
        let action = match connected {
            VDS_CONNECTED_KEEP => ConnectedAction::Keep,
            VDS_CONNECTED_FAIL => ConnectedAction::Fail,
            VDS_CONNECTED_DISCONNECT => ConnectedAction::Disconnect,
            _ => return VDS_E_INVALID_ARG,
        };
        let creds = pass.map(|p| Credentials {
            username: user.unwrap_or_default(),
            password: Secret::new(p),
            domain: domain.filter(|d| !d.is_empty()),
        });
        match s.core.set_password(creds, one_time, action) {
            Ok(()) => VDS_OK,
            Err(e) => {
                // SAFETY: `err` per the function contract.
                unsafe { set_err(err, e) };
                VDS_E_STATE
            }
        }
    })
}

/// Sets when the password expires, in seconds since the Unix epoch (0 or past = now,
/// `VDS_EXPIRE_NEVER` = never) (QMP `expire_password`). `[main-thread]`
///
/// # Safety
/// `server` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_set_password_expiry(server: *mut VdsServer, unix_time: i64) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return VDS_E_INVALID_ARG;
        };
        let at = (unix_time != VDS_EXPIRE_NEVER)
            .then(|| UNIX_EPOCH + Duration::from_secs(u64::try_from(unix_time).unwrap_or(0)));
        s.core.credentials().set_expiry(at);
        VDS_OK
    })
}

/// Copies information about up to `max` connected clients into `out` and returns the total
/// number of clients. `[any-thread]`
///
/// # Safety
/// `server` must be valid; `out` must be NULL (with `max == 0`) or writable for `max` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_clients(server: *mut VdsServer, out: *mut VdsClientInfo, max: u32) -> u32 {
    ffi_guard!(0, {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return 0;
        };
        let clients = s.core.clients();
        if !out.is_null() {
            for (i, c) in clients.iter().take(max as usize).enumerate() {
                // SAFETY: `out` is writable for `max` entries and `i < max`.
                unsafe { out.add(i).write(client_info(c)) };
            }
        }
        u32::try_from(clients.len()).unwrap_or(u32::MAX)
    })
}

/// Whether frames should be submitted now (a client is connected and not backpressured).
/// Submitting anyway is allowed. `[any-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_server_wants_frame(server: *mut VdsServer) -> bool {
    ffi_guard!(false, {
        // SAFETY: per the function contract.
        unsafe { as_server(server) }.is_some_and(|s| s.core.display().wants_frame())
    })
}

/// Adds display head `index` (QEMU console) of `width`x`height`. `[main-thread]`
///
/// # Safety
/// `server` must be valid; `err` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_add(
    server: *mut VdsServer,
    index: u32,
    width: u32,
    height: u32,
    err: *mut *mut c_char,
) -> *mut VdsHead {
    ffi_guard!(core::ptr::null_mut(), {
        // SAFETY: per the function contract.
        let Some(s) = (unsafe { as_server(server) }) else {
            return core::ptr::null_mut();
        };
        match s.core.display().add_head(index, width, height) {
            Ok(h) => Box::into_raw(Box::new(VdsHead {
                core: s.core.clone(),
                head: h,
            })),
            Err(e) => {
                // SAFETY: `err` per the function contract.
                unsafe { set_err(err, e) };
                core::ptr::null_mut()
            }
        }
    })
}

/// Changes the size of a head; its content becomes black until the next submit.
/// `[any-thread]`
///
/// # Safety
/// `head` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_resize(head: *mut VdsHead, width: u32, height: u32) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(h) = (unsafe { as_head(head) }) else {
            return VDS_E_INVALID_ARG;
        };
        match h.head.resize(width, height) {
            Ok(()) => VDS_OK,
            Err(_) => VDS_E_INVALID_ARG,
        }
    })
}

/// Copies the damaged `rects` of `bgrx` (`len` bytes, `stride` bytes per row, head
/// geometry, 32-bit BGRX little endian) into libvds. Returns after the copy. `[any-thread]`
///
/// # Safety
/// `head` must be valid; `bgrx` readable for `len` bytes; `rects` readable for `count`
/// entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_submit(
    head: *mut VdsHead,
    bgrx: *const u8,
    len: usize,
    stride: u32,
    rects: *const VdsRect,
    count: u32,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let (Some(h), Some(px), Some(rs)) = (unsafe { as_head(head) }, unsafe { slice(bgrx, len) }, unsafe {
            slice(rects, count as usize)
        }) else {
            return VDS_E_INVALID_ARG;
        };
        let rects: Vec<Rect> = rs.iter().map(|r| Rect::new(r.x, r.y, r.w, r.h)).collect();
        match h.head.submit(px, stride as usize, &rects) {
            Ok(()) => VDS_OK,
            Err(e) => {
                vds_core::warn_ratelimited!(error = %e, head = h.head.index(), "rejected frame submission");
                VDS_E_INVALID_ARG
            }
        }
    })
}

/// A host framebuffer read in place (`vds_head_attach_shared()`).
struct HostPixels {
    ptr: *const u8,
    len: usize,
    release: VdsReleaseFn,
    opaque: *mut c_void,
    /// Cleared when the attach fails: the host keeps the buffer and is not called back.
    armed: std::sync::atomic::AtomicBool,
}

// SAFETY: the host keeps `ptr` readable for `len` bytes until `release` is called, and
// `release` may be called from any thread (vds_head_attach_shared() contract). libvds only
// reads through the pointer.
unsafe impl Send for HostPixels {}
// SAFETY: see above; shared access is read-only.
unsafe impl Sync for HostPixels {}

impl std::fmt::Debug for HostPixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostPixels").field("ptr", &self.ptr).field("len", &self.len).finish()
    }
}

impl vds_core::display::SharedPixels for HostPixels {
    fn bytes(&self) -> &[u8] {
        // SAFETY: non-NULL (checked in vds_head_attach_shared()) and readable for `len`
        // bytes until `release`, which only runs when this value is dropped. The host may
        // write the buffer concurrently; libvds accepts the resulting tearing (the next
        // frame's damage repairs it) and never relies on the bytes staying the same.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for HostPixels {
    fn drop(&mut self) {
        if !self.armed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if let Some(release) = self.release {
            // SAFETY: the host's callback, called once with its own context pointer.
            unsafe { release(self.opaque) };
        }
    }
}

/// Makes libvds read the head's pixels in place from `bgrx` (`len` bytes, `stride` bytes per
/// row, head geometry, 32-bit BGRX little endian) instead of copying them on every submit.
/// The host then reports changes with `vds_head_damage()`. `[any-thread]`
///
/// libvds reads the buffer from its own threads, whenever it encodes a frame; the host may
/// keep drawing into it, at the price of an occasional torn frame. When libvds no longer needs
/// the buffer — another one is attached, the head is resized or removed, or
/// `vds_head_submit()` switches back to copying — it calls `release(opaque)` once, from any
/// thread. On error `release` is not called and the host keeps the buffer.
///
/// # Safety
/// `head` must be valid; `bgrx` readable for `len` bytes until `release` is called; `release`
/// NULL or callable from any thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_attach_shared(
    head: *mut VdsHead,
    bgrx: *const u8,
    len: usize,
    stride: u32,
    release: VdsReleaseFn,
    opaque: *mut c_void,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let Some(h) = (unsafe { as_head(head) }) else {
            return VDS_E_INVALID_ARG;
        };
        if bgrx.is_null() || len == 0 {
            return VDS_E_INVALID_ARG;
        }
        let pixels = Arc::new(HostPixels {
            ptr: bgrx,
            len,
            release,
            opaque,
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        match h.head.attach_shared(pixels.clone(), stride as usize) {
            Ok(()) => VDS_OK,
            Err(e) => {
                // The head dropped its reference already; this one is the last.
                pixels.armed.store(false, std::sync::atomic::Ordering::Release);
                vds_core::warn_ratelimited!(error = %e, head = h.head.index(), "rejected shared framebuffer");
                VDS_E_INVALID_ARG
            }
        }
    })
}

/// Reports damaged `rects` of the head without copying pixels: libvds reads them from the
/// buffer given to `vds_head_attach_shared()`. `[any-thread]`
///
/// # Safety
/// `head` must be valid; `rects` readable for `count` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_damage(head: *mut VdsHead, rects: *const VdsRect, count: u32) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        // SAFETY: per the function contract.
        let (Some(h), Some(rs)) = (unsafe { as_head(head) }, unsafe { slice(rects, count as usize) }) else {
            return VDS_E_INVALID_ARG;
        };
        let rects: Vec<Rect> = rs.iter().map(|r| Rect::new(r.x, r.y, r.w, r.h)).collect();
        h.head.damage(&rects);
        VDS_OK
    })
}

/// Removes a head and frees the handle. `[main-thread]`
///
/// # Safety
/// `head` must be NULL or a handle from `vds_head_add()`; it must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_head_remove(head: *mut VdsHead) {
    ffi_guard!((), {
        if head.is_null() {
            return;
        }
        // SAFETY: the pointer came from `Box::into_raw` in `vds_head_add()`.
        let h = unsafe { Box::from_raw(head) };
        h.core.display().remove_head(h.head.index());
    })
}

/// Defines the cursor shape: `width`x`height` pixels 0xAARRGGBB (straight alpha), top-down.
/// `[any-thread]`
///
/// # Safety
/// `server` must be valid; `argb` readable for `width * height` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_cursor_define(
    server: *mut VdsServer,
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
    argb: *const u32,
) -> i32 {
    ffi_guard!(VDS_E_INTERNAL, {
        let n = width as usize * height as usize;
        // SAFETY: per the function contract.
        let (Some(s), Some(px)) = (unsafe { as_server(server) }, unsafe { slice(argb, n) }) else {
            return VDS_E_INVALID_ARG;
        };
        match CursorImage::new(width, height, hot_x, hot_y, px.to_vec()) {
            Ok(img) => {
                s.core.cursor().define(img);
                VDS_OK
            }
            Err(_) => VDS_E_INVALID_ARG,
        }
    })
}

/// Shows or hides the cursor. `[any-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_cursor_set_visible(server: *mut VdsServer, visible: bool) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let Some(s) = unsafe { as_server(server) } {
            s.core.cursor().set_visible(visible);
        }
    })
}

/// Plays `count` interleaved samples (48 kHz, s16, stereo). `[any-thread]`
///
/// # Safety
/// `server` must be valid; `pcm` readable for `count` samples.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_audio_out(server: *mut VdsServer, pcm: *const i16, count: usize) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let (Some(s), Some(samples)) = (unsafe { as_server(server) }, unsafe { slice(pcm, count) }) {
            s.core.audio_out(samples);
        }
    })
}

/// Sets the playback volume (0..=65535 per channel). `[any-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_audio_volume(server: *mut VdsServer, left: u16, right: u16, mute: bool) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let Some(s) = unsafe { as_server(server) } {
            s.core.audio_volume(left, right, mute);
        }
    })
}

fn clipboard_types(raw: &[u32]) -> Vec<ClipboardType> {
    raw.iter().filter_map(|t| ClipboardType::from_raw(*t)).collect()
}

/// The guest clipboard holds data of these `VDS_CLIPBOARD_*` types. `[any-thread]`
///
/// # Safety
/// `server` must be valid; `types` readable for `count` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_clipboard_grab(server: *mut VdsServer, types: *const u32, count: u32) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let (Some(s), Some(t)) = (unsafe { as_server(server) }, unsafe { slice(types, count as usize) }) {
            let types = clipboard_types(t);
            if types.is_empty() {
                s.core.clipboard_release();
            } else {
                s.core.clipboard_grab(types);
            }
        }
    })
}

/// The guest clipboard is empty. `[any-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_clipboard_release(server: *mut VdsServer) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let Some(s) = unsafe { as_server(server) } {
            s.core.clipboard_release();
        }
    })
}

/// The guest wants the client clipboard; the answer arrives through `clipboard_data`.
/// `[any-thread]`
///
/// # Safety
/// `server` must be NULL or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_clipboard_request(server: *mut VdsServer, ty: u32) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        if let (Some(s), Some(t)) = (unsafe { as_server(server) }, ClipboardType::from_raw(ty)) {
            s.core.clipboard_request(t);
        }
    })
}

/// Guest clipboard content requested by the client (`clipboard_request` callback).
/// `[any-thread]`
///
/// # Safety
/// `server` must be valid; `data` readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vds_clipboard_data(server: *mut VdsServer, ty: u32, data: *const u8, len: usize) {
    ffi_guard!((), {
        // SAFETY: per the function contract.
        // SAFETY: per the function contract.
        let s = unsafe { as_server(server) };
        // SAFETY: per the function contract.
        let d = unsafe { slice(data, len) };
        if let (Some(s), Some(t), Some(d)) = (s, ClipboardType::from_raw(ty), d) {
            s.core.clipboard_data(t, d.to_vec());
        }
    })
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static WAKEUPS: AtomicU32 = AtomicU32::new(0);
    static WHEELS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn on_wakeup(_op: *mut c_void) {
        WAKEUPS.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn on_wheel(op: *mut c_void, _dx: i32, dy: i32) {
        assert_eq!(op as usize, 0x1234);
        WHEELS.fetch_add(u32::try_from(dy).unwrap(), Ordering::SeqCst);
    }

    fn new_server() -> *mut VdsServer {
        let mut cfg = core::mem::MaybeUninit::<VdsConfig>::uninit();
        // SAFETY: `cfg` is writable.
        unsafe { vds_config_init(cfg.as_mut_ptr()) };
        // SAFETY: initialized above.
        let cfg = unsafe { cfg.assume_init() };
        let cb = VdsCallbacks {
            wakeup: Some(on_wakeup),
            wheel: Some(on_wheel),
            ..VdsCallbacks::default()
        };
        let mut err = core::ptr::null_mut();
        // SAFETY: valid config and callbacks.
        let s = unsafe { vds_server_new(&cfg, &cb, 0x1234 as *mut c_void, &mut err) };
        assert!(!s.is_null());
        s
    }

    #[test]
    fn version() {
        // The major is part of the contract; the minor moves on compatible additions.
        let version = vds_abi_version();
        assert_eq!(version >> 16, 1, "ABI major changed");
        assert_eq!(version & 0xffff, crate::VDS_ABI_MINOR, "ABI minor mismatch");
    }

    #[test]
    fn events_are_dispatched_through_callbacks() {
        let s = new_server();
        // SAFETY: `s` is valid.
        let core = unsafe { &(*s).core };
        core.push_event(ClientEvent::Wheel { dx: 0, dy: 3 });
        assert!(WAKEUPS.load(Ordering::SeqCst) >= 1);
        // SAFETY: `s` is valid.
        unsafe { vds_server_dispatch(s) };
        assert_eq!(WHEELS.load(Ordering::SeqCst), 3);
        // SAFETY: `s` is valid and not used afterwards.
        unsafe { vds_server_free(s) };
    }

    #[test]
    fn head_submit_validates_buffers() {
        let s = new_server();
        // SAFETY: valid server.
        let h = unsafe { vds_head_add(s, 0, 4, 4, core::ptr::null_mut()) };
        assert!(!h.is_null());
        let px = [0u8; 64];
        let r = VdsRect { x: 0, y: 0, w: 4, h: 4 };
        // SAFETY: valid buffers.
        assert_eq!(unsafe { vds_head_submit(h, px.as_ptr(), px.len(), 16, &r, 1) }, VDS_OK);
        // SAFETY: buffer too short is reported, not read out of bounds.
        let rc = unsafe { vds_head_submit(h, px.as_ptr(), 10, 16, &r, 1) };
        assert_eq!(rc, VDS_E_INVALID_ARG);
        // SAFETY: NULL with non-zero length is rejected.
        let rc = unsafe { vds_head_submit(h, core::ptr::null(), 64, 16, &r, 1) };
        assert_eq!(rc, VDS_E_INVALID_ARG);
        // SAFETY: handles are valid and not used afterwards.
        unsafe {
            vds_head_remove(h);
            vds_server_free(s);
        }
    }

    #[test]
    fn bad_config_is_reported() {
        let mut cfg = core::mem::MaybeUninit::<VdsConfig>::uninit();
        // SAFETY: `cfg` is writable.
        unsafe { vds_config_init(cfg.as_mut_ptr()) };
        // SAFETY: initialized above.
        let mut cfg = unsafe { cfg.assume_init() };
        cfg.codecs = 0;
        let mut err = core::ptr::null_mut();
        // SAFETY: valid pointers.
        let s = unsafe { vds_server_new(&cfg, core::ptr::null(), core::ptr::null_mut(), &mut err) };
        assert!(s.is_null());
        assert!(!err.is_null());
        // SAFETY: `err` came from libvds.
        let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        assert!(msg.contains("codec"), "{msg}");
        // SAFETY: `err` came from libvds.
        unsafe { vds_free_string(err) };
    }

    /// The quantizer fields reach `ServerConfig` and an impossible pair is
    /// refused here rather than at the first frame.
    #[test]
    fn quantizer_fields_are_validated() {
        let init = || {
            let mut cfg = core::mem::MaybeUninit::<VdsConfig>::uninit();
            // SAFETY: `cfg` is writable.
            unsafe { vds_config_init(cfg.as_mut_ptr()) };
            // SAFETY: initialized above.
            unsafe { cfg.assume_init() }
        };
        let start = |cfg: &VdsConfig| -> Result<*mut VdsServer, String> {
            let mut err = core::ptr::null_mut();
            // SAFETY: valid config and error pointer.
            let s = unsafe { vds_server_new(cfg, core::ptr::null(), core::ptr::null_mut(), &mut err) };
            if s.is_null() {
                // SAFETY: `err` came from libvds.
                Err(unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned())
            } else {
                Ok(s)
            }
        };

        // 0 selects the defaults, as `encoder_threads` does.
        let mut cfg = init();
        assert!(cfg.qp_max > 0 && cfg.refine_qp > 0, "init must write the defaults");
        cfg.qp_max = 0;
        cfg.refine_qp = 0;
        let s = start(&cfg).unwrap();
        // SAFETY: valid server, not used afterwards.
        unsafe { vds_server_free(s) };

        let mut cfg = init();
        cfg.qp_max = 99;
        let msg = start(&cfg).unwrap_err();
        assert!(msg.contains("qp-max"), "{msg}");

        let mut cfg = init();
        cfg.qp_max = 20;
        cfg.refine_qp = 30;
        let msg = start(&cfg).unwrap_err();
        assert!(msg.contains("refine-qp"), "{msg}");
    }

    #[test]
    fn copy_str_truncates_on_char_boundary() {
        let mut buf = [1 as c_char; 4];
        copy_str(&mut buf, "aé€");
        assert_eq!(buf[3], 0);
        assert_eq!(buf[0] as u8, b'a');
    }
}
