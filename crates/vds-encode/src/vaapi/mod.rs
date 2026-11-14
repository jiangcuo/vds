//! H.264 on the GPU through VA-API: FFmpeg's `h264_vaapi`, fed by the VA video processor that
//! turns BGRX into full-range BT.709 NV12.
//!
//! A host GPU buffer (dmabuf, [`GpuFrame`]) is imported and read by the video processor in
//! place, without an upload ([`VaapiEncoder::encode_gpu`]); [`VaapiProbe`] checks up front
//! that a buffer can be used that way.
//!
//! Built only with the `vaapi` feature on Linux, and only when pkg-config finds libavcodec,
//! libavutil and libva; otherwise [`VaapiEncoder::open`] returns [`Error::Unavailable`] and
//! the caller keeps openh264.
//!
//! [`Error::Unavailable`]: crate::Error::Unavailable
//! [`GpuFrame`]: vds_core::display::GpuFrame

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

pub use imp::{VaapiEncoder, VaapiProbe};

#[cfg(vds_vaapi)]
mod imp {
    use core::ffi::{c_char, c_int};
    use std::ffi::{CStr, CString};
    use std::ptr::NonNull;

    use vds_core::display::{GpuBuffer, GpuFrame};

    use super::{NOMINAL_QP, VaapiConfig};
    use crate::bgrx::BgrxFrame;
    use crate::error::{Error, Result};
    use crate::EncodedFrame;

    #[repr(C)]
    struct RawDmabuf {
        fd: i32,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
    }

    impl RawDmabuf {
        fn new(buffer: &GpuBuffer) -> Self {
            Self {
                fd: buffer.fd.raw(),
                width: buffer.width,
                height: buffer.height,
                stride: buffer.stride,
                fourcc: buffer.fourcc,
                modifier: buffer.modifier,
            }
        }
    }

    #[repr(C)]
    struct RawProbe {
        _private: [u8; 0],
    }

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
        fn vds_vaapi_encode_dmabuf(
            h: *mut RawHandle,
            buf: *const RawDmabuf,
            generation: u64,
            flip: i32,
            force_idr: c_int,
            pts_ms: i64,
            out: *mut *const u8,
        ) -> i32;
        fn vds_vaapi_set_rate(h: *mut RawHandle, bitrate_kbps: i32) -> i32;
        fn vds_vaapi_close(h: *mut RawHandle);
        fn vds_vaapi_probe_open(render_node: *const c_char, err: *mut c_char, err_len: usize) -> *mut RawProbe;
        fn vds_vaapi_probe_check(
            p: *mut RawProbe,
            buf: *const RawDmabuf,
            flip: i32,
            err: *mut c_char,
            err_len: usize,
        ) -> i32;
        fn vds_vaapi_probe_close(p: *mut RawProbe);
    }

    /// Turns the shim's error buffer into a string.
    fn err_string(err: &[c_char]) -> String {
        // SAFETY: the shim always NUL-terminates `err` within its length, and the buffer is
        // zero-initialised by every caller, so a NUL is present either way.
        unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned()
    }

    /// Copies an access unit the shim returned.
    ///
    /// # Safety
    /// `out` must point to `size` readable bytes when `size > 0`.
    unsafe fn take_output(size: i32, out: *const u8, idr: bool) -> Result<Option<EncodedFrame>> {
        if size < 0 {
            return Err(Error::Encode);
        }
        if size == 0 || out.is_null() {
            return Ok(None);
        }
        let len = usize::try_from(size).map_err(|_| Error::Encode)?;
        // SAFETY: per the function contract; the bytes are copied before returning.
        let data = unsafe { core::slice::from_raw_parts(out, len) }.to_vec();
        Ok(Some(EncodedFrame {
            data,
            quality: NOMINAL_QP,
            idr,
        }))
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
                None => Err(Error::Open(err_string(&err))),
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
            // SAFETY: the shim returned `size` bytes at `out`, valid until the next call on
            // this encoder.
            unsafe { take_output(size, out, force_idr) }
        }

        /// Encodes the host GPU buffer of `frame` without reading it on the CPU: it is imported
        /// (once per generation) and converted by the video processor, flipped when its rows
        /// are bottom-up. [`Error::Unsupported`] when this device cannot use the buffer.
        pub fn encode_gpu(&mut self, frame: &GpuFrame, force_idr: bool, pts_ms: i64) -> Result<Option<EncodedFrame>> {
            let raw = RawDmabuf::new(&frame.buffer);
            let mut out: *const u8 = core::ptr::null();
            // SAFETY: `raw` outlives the call and its fd is open while `frame` lives; `out` is
            // a valid writable location.
            let size = unsafe {
                vds_vaapi_encode_dmabuf(
                    self.handle.as_ptr(),
                    &raw,
                    frame.generation,
                    i32::from(frame.buffer.y0_top),
                    c_int::from(force_idr),
                    pts_ms,
                    &mut out,
                )
            };
            if size == -2 {
                return Err(Error::Unsupported(String::from(
                    "VA-API cannot import or flip the host GPU buffer",
                )));
            }
            // SAFETY: as in `encode`.
            unsafe { take_output(size, out, force_idr) }
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

    /// Checks that host GPU buffers can be encoded on a VA-API device.
    #[derive(Debug)]
    pub struct VaapiProbe {
        handle: NonNull<RawProbe>,
    }

    // SAFETY: the handle is only used through `&mut self`; the VA display is not tied to the
    // thread that opened it.
    unsafe impl Send for VaapiProbe {}

    impl VaapiProbe {
        /// Opens the device the encoder would use.
        pub fn open(render_node: Option<&str>) -> Result<Self> {
            let node = render_node
                .map(CString::new)
                .transpose()
                .map_err(|_| Error::Open(String::from("render node")))?;
            let mut err = [0 as c_char; 256];
            // SAFETY: the node string outlives the call; `err` is writable for its length.
            let handle = unsafe {
                vds_vaapi_probe_open(
                    node.as_ref().map_or(core::ptr::null(), |n| n.as_ptr()),
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            NonNull::new(handle)
                .map(|handle| Self { handle })
                .ok_or_else(|| Error::Open(err_string(&err)))
        }

        /// Imports `buffer` once and checks the video processor can read (and flip) it.
        pub fn check(&mut self, buffer: &GpuBuffer) -> Result<()> {
            let raw = RawDmabuf::new(buffer);
            let mut err = [0 as c_char; 256];
            // SAFETY: `raw` outlives the call and its fd is open while `buffer` lives; `err` is
            // writable for its length.
            let rc = unsafe {
                vds_vaapi_probe_check(
                    self.handle.as_ptr(),
                    &raw,
                    i32::from(buffer.y0_top),
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if rc < 0 {
                return Err(Error::Unsupported(err_string(&err)));
            }
            Ok(())
        }
    }

    impl Drop for VaapiProbe {
        fn drop(&mut self) {
            // SAFETY: the handle came from `vds_vaapi_probe_open` and is closed exactly once.
            unsafe { vds_vaapi_probe_close(self.handle.as_ptr()) }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::{FromRawFd as _, OwnedFd};

        use vds_core::display::{Display, GpuBuffer};
        use vds_core::geom::Rect;
        use vds_core::platform::BufferFd;

        use super::{RawDmabuf, RawProbe, VaapiConfig, VaapiEncoder, VaapiProbe};

        unsafe extern "C" {
            fn vds_vaapi_test_dmabuf(p: *mut RawProbe, width: u32, height: u32, rgbx: u32, out: *mut RawDmabuf) -> i32;
        }

        /// A GPU buffer filled with one colour, from the probe's device.
        fn test_buffer(probe: &mut VaapiProbe, w: u32, h: u32, y0_top: bool) -> Option<GpuBuffer> {
            let mut raw = RawDmabuf {
                fd: -1,
                width: 0,
                height: 0,
                stride: 0,
                fourcc: 0,
                modifier: 0,
            };
            // SAFETY: the probe handle is valid and `raw` is writable.
            let fd = unsafe { vds_vaapi_test_dmabuf(probe.handle.as_ptr(), w, h, 0x0000_ff00, &mut raw) };
            if fd < 0 {
                return None;
            }
            Some(GpuBuffer {
                // SAFETY: the shim exported a new descriptor that nothing else owns.
                fd: BufferFd::new(unsafe { OwnedFd::from_raw_fd(fd) }),
                width: raw.width,
                height: raw.height,
                stride: raw.stride,
                fourcc: raw.fourcc,
                modifier: raw.modifier,
                y0_top,
            })
        }

        #[test]
        fn encodes_a_gpu_buffer_without_the_cpu() {
            // Without a VA-API device (CI) there is nothing to test.
            let Ok(mut probe) = VaapiProbe::open(None) else {
                return;
            };
            let Some(buffer) = test_buffer(&mut probe, 320, 240, true) else {
                return;
            };
            probe.check(&buffer).unwrap();

            let display = Display::new();
            let head = display.add_head(0, 320, 240).unwrap();
            display.add_consumer(1);
            head.attach_gpu(buffer, None).unwrap();
            let mut enc = VaapiEncoder::open(&VaapiConfig {
                width: 320,
                height: 240,
                fps: 30,
                render_node: None,
                bitrate_kbps: 2000,
            })
            .unwrap();
            let mut bytes = 0;
            for i in 0..3 {
                head.damage(&[Rect::new(0, 0, 320, 240)]);
                let frame = head.take_frame(1, |f, _, _| f.gpu.unwrap());
                if let Some(out) = enc.encode_gpu(&frame, i == 0, i64::from(i) * 33).unwrap() {
                    bytes += out.data.len();
                }
            }
            assert!(bytes > 0);
        }

        #[test]
        fn probe_refuses_an_unknown_format() {
            // Without a VA-API device (CI) there is nothing to test.
            let Ok(mut probe) = VaapiProbe::open(None) else {
                return;
            };
            let Some(mut buffer) = test_buffer(&mut probe, 64, 64, false) else {
                return;
            };
            buffer.fourcc = 0x3231_564e; // NV12: not a 32-bit RGB format
            assert!(probe.check(&buffer).is_err());
        }
    }
}

#[cfg(not(vds_vaapi))]
mod imp {
    use vds_core::display::{GpuBuffer, GpuFrame};

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
        pub fn encode_gpu(
            &mut self,
            _frame: &GpuFrame,
            _force_idr: bool,
            _pts_ms: i64,
        ) -> Result<Option<EncodedFrame>> {
            Err(Error::Unavailable)
        }

        /// Unreachable: no encoder can be opened.
        pub fn set_rate(&mut self, _bitrate_kbps: u32) -> Result<()> {
            Err(Error::Unavailable)
        }
    }

    /// Placeholder used when libvds is built without VA-API.
    #[derive(Debug)]
    pub struct VaapiProbe {
        _private: (),
    }

    impl VaapiProbe {
        /// Always fails: this build has no VA-API.
        pub fn open(_render_node: Option<&str>) -> Result<Self> {
            Err(Error::Unavailable)
        }

        /// Unreachable: no probe can be opened.
        pub fn check(&mut self, _buffer: &GpuBuffer) -> Result<()> {
            Err(Error::Unavailable)
        }
    }
}
