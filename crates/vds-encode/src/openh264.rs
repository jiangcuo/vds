//! H.264 on the CPU with openh264.
//!
//! Low complexity, no scene-change detection, IDR only on demand, full-range BT.709, and
//! quality rate control instead of a bitrate target:
//! the bitrate follows the content, a still screen costs almost nothing and a busy one takes
//! what it needs, bounded by the quantizer ceiling (`qp-max`). Frame skipping is
//! off: a frame the encoder dropped would leave the client's picture behind the damage it was
//! told about.

use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    QpRange, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

use crate::error::{Error, Result};
use crate::yuv::I420Frame;
use crate::{EncodedFrame, EncoderConfig, RateControl};

/// Bitrate used when rate control has no cap.
const DEFAULT_KBPS: u32 = 20_000;

/// The quantizer reported for openh264 frames: the encoder does not report a frame's average,
/// and the caller only uses it to tell whether an area went out coarsely.
const NOMINAL_QP: f64 = 23.0;

/// An openh264 encoder for `width`x`height` I420 pictures.
pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

fn kbps(rate: &RateControl) -> u32 {
    if rate.vbv_max_kbps == 0 {
        DEFAULT_KBPS
    } else {
        rate.vbv_max_kbps
    }
}

impl OpenH264Encoder {
    /// Opens an encoder for a `cfg.width`x`cfg.height` source (even dimensions).
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
            return Err(Error::InvalidSize(cfg.width, cfg.height));
        }
        let threads = u16::try_from(cfg.threads.max(1)).map_err(|_| Error::Open(String::from("threads")))?;
        let qp_max = if cfg.rate.qp_max == 0 { 51 } else { cfg.rate.qp_max };
        let config = OhConfig::new()
            .max_frame_rate(FrameRate::from_hz(cfg.fps.max(1) as f32))
            .rate_control_mode(RateControlMode::Quality)
            .bitrate(BitRate::from_bps(kbps(&cfg.rate).saturating_mul(1000)))
            .intra_frame_period(IntraFramePeriod::from_num_frames(i32::MAX as u32))
            .complexity(Complexity::Low)
            .num_threads(threads)
            .scene_change_detect(false)
            .background_detection(false)
            .adaptive_quantization(false)
            .skip_frames(false)
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .qp(QpRange::new(0, qp_max))
            .vui(VuiConfig::bt709_full());
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| Error::Open(e.to_string()))?;
        Ok(Self {
            encoder,
            width: cfg.width,
            height: cfg.height,
        })
    }

    /// Encodes a picture. openh264 takes no per-frame quantizer: `_qp` is ignored, a
    /// refinement frame is an ordinary one.
    pub fn encode(
        &mut self,
        pic: &I420Frame,
        force_idr: bool,
        _qp: Option<u8>,
        pts_ms: i64,
    ) -> Result<Option<EncodedFrame>> {
        if pic.width() < self.width || pic.height() < self.height {
            return Err(Error::SizeMismatch {
                got_w: pic.width(),
                got_h: pic.height(),
                want_w: self.width,
                want_h: self.height,
            });
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (ys, uvs) = (pic.y_stride(), pic.uv_stride());
        // The planes are padded to whole macroblocks; openh264 wants exactly the source rows
        // and crops the padding itself.
        let y = pic.y().get(..h * ys).ok_or(Error::Encode)?;
        let u = pic.u().get(..h / 2 * uvs).ok_or(Error::Encode)?;
        let v = pic.v().get(..h / 2 * uvs).ok_or(Error::Encode)?;
        let yuv = YUVSlices::new((y, u, v), (w, h), (ys, uvs, uvs));
        if force_idr {
            self.encoder.force_intra_frame();
        }
        let ts = Timestamp::from_millis(u64::try_from(pts_ms).unwrap_or(0));
        let bitstream = self.encoder.encode_at(&yuv, ts).map_err(|_| Error::Encode)?;
        let idr = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            quality: NOMINAL_QP,
            idr,
        }))
    }

    /// Rate changes are not applied: quality mode lets the bitrate follow the content, and the
    /// quantizer ceiling set at open keeps the picture from blurring.
    pub fn set_rate(&mut self, _rate: RateControl) -> Result<()> {
        Ok(())
    }
}
