//! H.264 on the GPU through VA-API: FFmpeg's `h264_vaapi`, fed by the VA video processor that
//! turns BGRX into full-range BT.709 NV12.
//!
//! Built only with the `vaapi` feature on Linux, and only when pkg-config finds libavcodec,
//! libavutil and libva; otherwise [`VaapiEncoder::open`] returns [`Error::Unavailable`] and
//! the caller keeps openh264.
//!
//! [`Error::Unavailable`]: crate::Error::Unavailable

/// What the VA-API encoder needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaapiConfig {
    /// Source width; the encoder crops its macroblock padding itself.
    pub width: u32,
    /// Source height.
    pub height: u32,
    /// Nominal frame rate.
    pub fps: u32,
    /// DRM render node (`/dev/dri/renderD128`); `None` lets FFmpeg pick one.
    pub render_node: Option<String>,
    /// Constant bitrate in kbit/s.
    pub bitrate_kbps: u32,
}

#[cfg(vds_vaapi)]
/// The quantizer reported for VA-API frames: the hardware does not say, and the caller only
/// uses it to decide whether an area was sent coarsely. 23 matches the software path.
const NOMINAL_QP: f64 = 23.0;

pub use imp::VaapiEncoder;

#[cfg(vds_vaapi)]
mod imp {
    use core::ffi::{c_char, c_int};
    use std::ffi::{CStr, CString};
    use std::ptr::NonNull;

    use super::{NOMINAL_QP, VaapiConfig};
    use crate::bgrx::BgrxFrame;
    use crate::error::{Error, Result};
    use crate::EncodedFrame;

    #[repr(C)]
    struct RawConfig {
        width: i32,
        height: i32,
        fps: i32,
        render_node: *const c_char,
        bitrate_kbps: i32,
    }

    #[repr(C)]
    struct RawHandle {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        fn vds_vaapi_open(cfg: *const RawConfig, err: *mut c_char, err_len: usize) -> *mut RawHandle;
        fn vds_vaapi_encode(
            h: *mut RawHandle,
            bgrx: *const u8,
            stride: i32,
            force_idr: c_int,
            pts_ms: i64,
            out: *mut *const u8,
        ) -> i32;
        fn vds_vaapi_set_rate(h: *mut RawHandle, bitrate_kbps: i32) -> i32;
        fn vds_vaapi_close(h: *mut RawHandle);
    }

    /// A VA-API H.264 encoder for `width`x`height` BGRX pictures.
    #[derive(Debug)]
    pub struct VaapiEncoder {
        handle: NonNull<RawHandle>,
        width: u32,
        height: u32,
    }

    // SAFETY: the handle is only used through `&mut self`, never from two threads at once;
    // FFmpeg's VA-API contexts are not tied to the thread that created them.
    unsafe impl Send for VaapiEncoder {}

    fn to_i32(v: u32) -> Result<i32> {
        i32::try_from(v).map_err(|_| Error::Open(format!("value {v} out of range")))
    }

    impl VaapiEncoder {
        /// Opens the device, the video processor and `h264_vaapi`.
        ///
        /// [`Error::Unavailable`] would come from a build without VA-API, never from here.
        pub fn open(cfg: &VaapiConfig) -> Result<Self> {
            let node = cfg
                .render_node
                .as_deref()
                .map(CString::new)
                .transpose()
                .map_err(|_| Error::Open(String::from("render node")))?;
            let raw = RawConfig {
                width: to_i32(cfg.width)?,
                height: to_i32(cfg.height)?,
                fps: to_i32(cfg.fps.max(1))?,
                render_node: node.as_ref().map_or(core::ptr::null(), |n| n.as_ptr()),
                bitrate_kbps: to_i32(cfg.bitrate_kbps)?,
            };
            let mut err = [0 as c_char; 256];
            // SAFETY: `raw` and the node string outlive the call; `err` is writable for its
            // length.
            let handle = unsafe { vds_vaapi_open(&raw, err.as_mut_ptr(), err.len()) };
            match NonNull::new(handle) {
                Some(handle) => Ok(Self {
                    handle,
                    width: cfg.width,
                    height: cfg.height,
                }),
                None => {
                    // SAFETY: the shim always NUL-terminates `err` within its length.
                    let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
                    Err(Error::Open(msg))
                }
            }
        }

        /// Encodes a picture.
        pub fn encode(&mut self, pic: &BgrxFrame, force_idr: bool, pts_ms: i64) -> Result<Option<EncodedFrame>> {
            if pic.width() != self.width || pic.height() != self.height {
                return Err(Error::SizeMismatch {
                    got_w: pic.width(),
                    got_h: pic.height(),
                    want_w: self.width,
                    want_h: self.height,
                });
            }
            let stride = i32::try_from(pic.stride()).map_err(|_| Error::Encode)?;
            let mut out: *const u8 = core::ptr::null();
            // SAFETY: `pic` holds `stride * height` bytes, read only during the call; `out`
            // is a valid writable location.
            let size = unsafe {
                vds_vaapi_encode(
                    self.handle.as_ptr(),
                    pic.data().as_ptr(),
                    stride,
                    c_int::from(force_idr),
                    pts_ms,
                    &mut out,
                )
            };
            if size < 0 {
                return Err(Error::Encode);
            }
            if size == 0 || out.is_null() {
                return Ok(None);
            }
            let len = usize::try_from(size).map_err(|_| Error::Encode)?;
            // SAFETY: the shim returned `size` bytes at `out`, valid until the next call on
            // this encoder; they are copied before returning.
            let data = unsafe { core::slice::from_raw_parts(out, len) }.to_vec();
            Ok(Some(EncodedFrame {
                data,
                quality: NOMINAL_QP,
                idr: force_idr,
            }))
        }

        /// Changes the bitrate.
        pub fn set_rate(&mut self, bitrate_kbps: u32) -> Result<()> {
            let kbps = i32::try_from(bitrate_kbps).map_err(|_| Error::Reconfig)?;
            // SAFETY: the handle is valid for the lifetime of `self`.
            let rc = unsafe { vds_vaapi_set_rate(self.handle.as_ptr(), kbps) };
            if rc < 0 { Err(Error::Reconfig) } else { Ok(()) }
        }
    }

    impl Drop for VaapiEncoder {
        fn drop(&mut self) {
            // SAFETY: the handle came from `vds_vaapi_open` and is closed exactly once.
            unsafe { vds_vaapi_close(self.handle.as_ptr()) }
        }
    }
}

#[cfg(not(vds_vaapi))]
mod imp {
    use super::VaapiConfig;
    use crate::bgrx::BgrxFrame;
    use crate::error::{Error, Result};
    use crate::EncodedFrame;

    /// Placeholder used when libvds is built without VA-API.
    #[derive(Debug)]
    pub struct VaapiEncoder {
        _private: (),
    }

    impl VaapiEncoder {
        /// Always fails: this build has no VA-API encoder.
        pub fn open(_cfg: &VaapiConfig) -> Result<Self> {
            Err(Error::Unavailable)
        }

        /// Unreachable: no encoder can be opened.
        pub fn encode(&mut self, _pic: &BgrxFrame, _force_idr: bool, _pts_ms: i64) -> Result<Option<EncodedFrame>> {
            Err(Error::Unavailable)
        }

        /// Unreachable: no encoder can be opened.
        pub fn set_rate(&mut self, _bitrate_kbps: u32) -> Result<()> {
            Err(Error::Unavailable)
        }
    }
}
