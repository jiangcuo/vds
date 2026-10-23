//! Video encoding for libvds.
//!
//! openh264 is the default encoder ([`OpenH264Encoder`]). With the `vaapi` feature on Linux, FFmpeg's `h264_vaapi` encodes on the GPU,
//! colour conversion included ([`vaapi`]). The [`VideoEncoder`] trait is kept so other protocols
//! can share the pipeline.

pub mod bgrx;
pub mod error;
pub mod openh264;
pub mod vaapi;
pub mod yuv;

pub use crate::bgrx::BgrxFrame;
pub use crate::error::{Error, Result};
pub use crate::openh264::OpenH264Encoder;
pub use crate::vaapi::{VaapiConfig, VaapiEncoder};
pub use crate::yuv::I420Frame;

/// Rate-control parameters: a bitrate cap and a quantizer ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// CRF times ten (230 = CRF 23); kept for encoders with a quality mode, openh264 uses the
    /// bitrate.
    pub crf_x10: u32,
    /// Highest quantizer the encoder may use; 0 leaves its own limit (51).
    ///
    /// Rate control on its own has no ceiling: one large change is enough for VBV to push the
    /// quantizer towards 51 and the picture turns to mush. libvds prefers a lower frame rate over lower quality, so the
    /// quantizer is capped and the encoder is not made to squeeze a frame into bits that
    /// are not there.
    pub qp_max: u8,
    /// VBV maximum bitrate in kbit/s; 0 disables the cap.
    pub vbv_max_kbps: u32,
    /// VBV buffer in kbit; 0 lets the encoder choose.
    pub vbv_buf_kbits: u32,
}

impl RateControl {
    /// CRF 23 capped at `max_kbps`, quantizer ceiling `qp_max`, half-second VBV buffer.
    pub fn capped(max_kbps: u32, fps: u32, qp_max: u8) -> Self {
        Self {
            crf_x10: 230,
            qp_max,
            vbv_max_kbps: max_kbps,
            vbv_buf_kbits: if max_kbps == 0 {
                0
            } else {
                // Half a second, but never less than one frame at `fps`.
                (max_kbps / 2).max(max_kbps / fps.max(1)).max(100)
            },
        }
    }
}

/// Encoder configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Width of the source, in pixels. Even and non-zero.
    ///
    /// The encoded picture is padded to a macroblock multiple and the extra columns are
    /// signalled as H.264 frame cropping, so decoders produce a picture of exactly this size.
    pub width: u32,
    /// Height of the source, in pixels. Even and non-zero; see [`Self::width`].
    pub height: u32,
    /// Nominal frame rate (timestamps decide the actual rate).
    pub fps: u32,
    /// Encoder threads.
    pub threads: u32,
    /// Rate control.
    pub rate: RateControl,
}

/// One encoded access unit.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedFrame {
    /// Annex-B H.264 bytes (SPS/PPS included on IDR frames).
    pub data: Vec<u8>,
    /// Average effective quantizer of the frame (0-51).
    pub quality: f64,
    /// The frame is an IDR.
    pub idr: bool,
}

/// A video encoder working on I420 pictures.
pub trait VideoEncoder: Send {
    /// Encodes a picture; `qp` forces the quantizer for this frame.
    fn encode(&mut self, pic: &I420Frame, force_idr: bool, qp: Option<u8>, pts_ms: i64)
    -> Result<Option<EncodedFrame>>;
    /// Changes rate control.
    fn set_rate(&mut self, rate: RateControl) -> Result<()>;
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(
        &mut self,
        pic: &I420Frame,
        force_idr: bool,
        qp: Option<u8>,
        pts_ms: i64,
    ) -> Result<Option<EncodedFrame>> {
        OpenH264Encoder::encode(self, pic, force_idr, qp, pts_ms)
    }

    fn set_rate(&mut self, rate: RateControl) -> Result<()> {
        OpenH264Encoder::set_rate(self, rate)
    }
}

#[cfg(test)]
mod tests {
    use vds_core::display::FrameRef;
    use vds_core::geom::Rect;

    use super::{EncoderConfig, I420Frame, OpenH264Encoder, RateControl};

    fn config(w: u32, h: u32) -> EncoderConfig {
        EncoderConfig {
            width: w,
            height: h,
            fps: 30,
            threads: 1,
            rate: RateControl::capped(4000, 30, 30),
        }
    }

    #[test]
    fn encodes_idr_with_start_code_then_small_p_frame() {
        let mut enc = OpenH264Encoder::open(&config(64, 64)).unwrap();
        let px: Vec<u8> = (0..64 * 64 * 4).map(|i| u8::try_from(i % 251).unwrap()).collect();
        let src = FrameRef {
            width: 64,
            height: 64,
            stride: 256,
            pixels: &px,
        };
        let mut pic = I420Frame::new(64, 64).unwrap();
        pic.convert(&src, Rect::new(0, 0, 64, 64)).unwrap();

        // No frame skipping: every picture yields an access unit.
        let idr = enc.encode(&pic, true, None, 0).unwrap().unwrap();
        assert_eq!(&idr.data[..4], &[0, 0, 0, 1]);
        assert!(idr.idr);

        let p = enc.encode(&pic, false, None, 33).unwrap().unwrap();
        assert!(
            p.data.len() < idr.data.len(),
            "static P frame should be smaller than the IDR"
        );

        let refined = enc.encode(&pic, false, Some(10), 66).unwrap().unwrap();
        assert!(!refined.data.is_empty());
    }

    #[test]
    fn rejects_odd_sizes() {
        assert!(OpenH264Encoder::open(&config(101, 64)).is_err());
        assert!(OpenH264Encoder::open(&config(64, 65)).is_err());
        assert!(OpenH264Encoder::open(&config(0, 64)).is_err());
    }

    /// The picture the client draws must be the display size, not the padded encode size:
    /// a H.264 frame taller than the surface is scaled by the client, which blurs every row
    /// (1080 becomes 1088 without it).
    #[test]
    fn crops_the_encode_padding_in_the_sps() {
        for (w, h) in [(1920, 1080), (1366, 768), (1024, 768)] {
            let mut enc = OpenH264Encoder::open(&config(w, h)).unwrap();
            let pic = I420Frame::new(w, h).unwrap();
            assert_eq!(pd(pic.width()), pd(w), "picture matches the encoder");
            let idr = enc.encode(&pic, true, None, 0).unwrap().unwrap();
            assert_eq!(sps_display_size(&idr.data), Some((w, h)), "{w}x{h}");
        }
    }

    /// Rounds up as `I420Frame` does.
    fn pd(v: u32) -> u32 {
        v.div_ceil(16) * 16
    }

    /// Splits Annex-B data into NAL units (without the start code).
    fn nal_units(data: &[u8]) -> Vec<&[u8]> {
        let mut units = Vec::new();
        let mut pos = 0;
        while let Some(off) = data[pos..].windows(3).position(|w| w == [0, 0, 1]) {
            let start = pos + off + 3;
            let end = data[start..]
                .windows(3)
                .position(|w| w == [0, 0, 1])
                // A 4-byte start code has a trailing zero that belongs to the next unit.
                .map_or(data.len(), |p| start + p - usize::from(data[start + p - 1] == 0));
            if start < end {
                units.push(&data[start..end]);
            }
            pos = end.max(start);
        }
        units
    }

    /// Strips the emulation prevention bytes of a NAL unit and its header (H.264 7.3.1).
    fn rbsp(nal: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal.iter().skip(1) {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Bits<'_> {
        fn bit(&mut self) -> Option<u32> {
            let byte = *self.data.get(self.pos / 8)?;
            let value = u32::from(byte) >> (7 - self.pos % 8) & 1;
            self.pos += 1;
            Some(value)
        }

        fn bits(&mut self, count: u32) -> Option<u32> {
            let mut value = 0;
            for _ in 0..count {
                value = value << 1 | self.bit()?;
            }
            Some(value)
        }

        /// Unsigned Exp-Golomb `ue(v)` (H.264 9.1).
        fn ue(&mut self) -> Option<u32> {
            let mut zeros = 0;
            while self.bit()? == 0 {
                zeros += 1;
                if zeros > 31 {
                    return None;
                }
            }
            let rest = self.bits(zeros)?;
            Some((1 << zeros) - 1 + rest)
        }

        /// Signed Exp-Golomb `se(v)` (H.264 9.1.1).
        fn se(&mut self) -> Option<i32> {
            let code = self.ue()?;
            let magnitude = i32::try_from(code.div_ceil(2)).ok()?;
            Some(if code % 2 == 0 { -magnitude } else { magnitude })
        }
    }

    /// The display size signalled by the SPS, after frame cropping (H.264 7.3.2.1.1).
    fn sps_display_size(annex_b: &[u8]) -> Option<(u32, u32)> {
        let sps = nal_units(annex_b)
            .into_iter()
            .find(|n| !n.is_empty() && n[0] & 0x1f == 7)?;
        let mut r = Bits {
            data: &rbsp(sps),
            pos: 0,
        };
        let profile = r.bits(8)?;
        r.bits(8)?; // constraint flags and reserved bits
        r.bits(8)?; // level_idc
        r.ue()?; // seq_parameter_set_id
        let mut chroma_format_idc = 1;
        if matches!(
            profile,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma_format_idc = r.ue()?;
            if chroma_format_idc == 3 {
                r.bit()?;
            }
            r.ue()?; // bit_depth_luma_minus8
            r.ue()?; // bit_depth_chroma_minus8
            r.bit()?; // qpprime_y_zero_transform_bypass_flag
            if r.bit()? == 1 {
                for i in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                    if r.bit()? == 1 {
                        // scaling_list (H.264 7.3.2.1.1.1)
                        let (mut last, mut next) = (8i32, 8i32);
                        for _ in 0..if i < 6 { 16 } else { 64 } {
                            if next != 0 {
                                next = (last + r.se()? + 256) % 256;
                            }
                            if next != 0 {
                                last = next;
                            }
                        }
                    }
                }
            }
        }
        r.ue()?; // log2_max_frame_num_minus4
        match r.ue()? {
            0 => {
                r.ue()?;
            }
            1 => {
                r.bit()?;
                r.se()?;
                r.se()?;
                for _ in 0..r.ue()? {
                    r.se()?;
                }
            }
            _ => {}
        }
        r.ue()?; // max_num_ref_frames
        r.bit()?; // gaps_in_frame_num_value_allowed_flag
        let width_mbs = r.ue()? + 1;
        let height_map_units = r.ue()? + 1;
        let frame_mbs_only = r.bit()?;
        if frame_mbs_only == 0 {
            r.bit()?;
        }
        r.bit()?; // direct_8x8_inference_flag
        let (mut width, mut height) = (width_mbs * 16, height_map_units * 16 * (2 - frame_mbs_only));
        if r.bit()? == 1 {
            let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
            let (sub_w, sub_h) = match chroma_format_idc {
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            width -= sub_w * (left + right);
            height -= sub_h * (2 - frame_mbs_only) * (top + bottom);
        }
        Some((width, height))
    }

    #[test]
    fn rate_change_is_accepted() {
        let mut enc = OpenH264Encoder::open(&config(64, 64)).unwrap();
        enc.set_rate(RateControl::capped(1000, 30, 30)).unwrap();
    }
}
