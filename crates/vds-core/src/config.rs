//! Server configuration (the `-rdp` options that are not about listening or TLS).

use crate::auth::AuthMode;
use crate::codec::CodecSet;
use crate::debug::DebugFlags;
use crate::error::{Error, Result};

/// Which H.264 encoder produces the AVC420 stream (`encoder=` on `-rdp`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoEncoderKind {
    /// openh264 on the CPU: always available, the default.
    #[default]
    OpenH264,
    /// FFmpeg `h264_vaapi` on the GPU, colour conversion included.
    /// Falls back to openh264 when the build or the host lacks it.
    Vaapi,
}

impl VideoEncoderKind {
    /// Parses `openh264` or `vaapi`.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "openh264" => Ok(Self::OpenH264),
            "vaapi" => Ok(Self::Vaapi),
            other => Err(Error::InvalidArgument(format!("unknown encoder `{other}` (openh264 or vaapi)"))),
        }
    }
}

/// What a client's "send Ctrl+Alt+Del" short-cut turns into.
///
/// RDP clients do not send Ctrl+Alt+Del itself — the client's own operating system would eat
/// it. They send Ctrl+Alt+End instead and leave the meaning to the server, which is what the
/// guest expects to receive: a secure attention sequence. Guests behind libvds get the
/// keystrokes, so the translation has to happen here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SasHotkey {
    /// Ctrl+Alt+End is injected into the guest as Ctrl+Alt+Del (default, as mstsc does).
    #[default]
    CtrlAltEnd,
    /// The keys are injected unchanged; the guest sees Ctrl+Alt+End.
    Off,
}

/// Server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Authentication mode.
    pub auth: AuthMode,
    /// Allowed codecs (order is fixed: avc420, remotefx, bitmap).
    pub codecs: CodecSet,
    /// Frame rate cap per display head.
    pub max_fps: u32,
    /// Maximum video bitrate in kbit/s; `0` selects automatic adaptation.
    pub bitrate_kbps: u32,
    /// openh264 threads per encoder; `0` selects the default (2).
    pub encoder_threads: u32,
    /// H.264 encoder.
    pub encoder: VideoEncoderKind,
    /// DRM render node of the VA-API encoder; `None` lets FFmpeg pick one.
    pub render_node: Option<String>,
    /// Highest quantizer the encoder may use (1-51, 51 being H.264's own limit).
    ///
    /// When the link or the decoder cannot keep
    /// up, libvds drops frames instead of letting VBV blur the picture. Without a ceiling
    /// a sustained bitrate deficit drives the quantizer towards 51.
    pub qp_max: u8,
    /// Quantizer of a refinement frame (1-51), used to re-send an area that was encoded
    /// too coarsely once it stops changing.
    pub refine_qp: u8,
    /// Delay after which static lossy areas are re-encoded at high quality; `0` disables.
    ///
    /// With `egfx_mixed` this is the time an area has to stop changing before it is re-sent
    /// losslessly instead, which is the same question asked of the same area.
    pub refine_delay_ms: u32,
    /// Send the still part of a head as lossless ClearCodec tiles beside its H.264 frame,
    /// which replaces the refinement frame of `refine_delay_ms`.
    pub egfx_mixed: bool,
    /// How long to wait for the client's EGFX channel before using legacy updates.
    pub egfx_grace_ms: u32,
    /// Enable audio playback (rdpsnd).
    pub audio: bool,
    /// Enable clipboard sharing (cliprdr).
    pub clipboard: bool,
    /// Ask the guest to adopt the client's resolution when it connects.
    ///
    /// On by default: the guest then matches the client window from the first frame instead
    /// of scaling the framebuffer. Set `resize-on-connect=off` on `-rdp` to keep the guest's
    /// own resolution.
    pub resize_on_connect: bool,
    /// How the client's Ctrl+Alt+End reaches the guest.
    pub sas_hotkey: SasHotkey,
    /// VM name or UUID, used in logs.
    pub vm_name: Option<String>,
    /// Directory receiving per-session diagnostic files; `None` disables debugging.
    ///
    /// See `docs/spec/logging.md` section 5 and [`crate::debug`].
    pub debug_dir: Option<String>,
    /// Optional traces written when `debug_dir` is set.
    pub debug_flags: DebugFlags,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            auth: AuthMode::Nla,
            codecs: CodecSet::ALL,
            max_fps: 60,
            bitrate_kbps: 0,
            encoder_threads: 0,
            encoder: VideoEncoderKind::OpenH264,
            render_node: None,
            qp_max: 30,
            refine_qp: 10,
            refine_delay_ms: 200,
            egfx_mixed: true,
            egfx_grace_ms: 1000,
            audio: true,
            clipboard: true,
            resize_on_connect: true,
            sas_hotkey: SasHotkey::default(),
            vm_name: None,
            debug_dir: None,
            debug_flags: DebugFlags::default(),
        }
    }
}

impl ServerConfig {
    /// Checks value ranges.
    pub fn validate(&self) -> Result<()> {
        if self.codecs.is_empty() {
            return Err(Error::InvalidConfig(String::from("no codec allowed")));
        }
        if !(1..=120).contains(&self.max_fps) {
            return Err(Error::InvalidConfig(format!(
                "max-fps {} out of range 1..=120",
                self.max_fps
            )));
        }
        if self.bitrate_kbps != 0 && !(100..=1_000_000).contains(&self.bitrate_kbps) {
            return Err(Error::InvalidConfig(format!(
                "bitrate {} kbit/s out of range 100..=1000000",
                self.bitrate_kbps
            )));
        }
        if !(1..=51).contains(&self.qp_max) {
            return Err(Error::InvalidConfig(format!(
                "qp-max {} out of range 1..=51",
                self.qp_max
            )));
        }
        if !(1..=51).contains(&self.refine_qp) {
            return Err(Error::InvalidConfig(format!(
                "refine-qp {} out of range 1..=51",
                self.refine_qp
            )));
        }
        if self.refine_qp > self.qp_max {
            return Err(Error::InvalidConfig(format!(
                "refine-qp {} above qp-max {}: a refinement frame would be worse than the frame it replaces",
                self.refine_qp, self.qp_max
            )));
        }
        if self.encoder_threads > 64 {
            return Err(Error::InvalidConfig(format!(
                "encoder-threads {} out of range 0..=64",
                self.encoder_threads
            )));
        }
        if self.egfx_grace_ms > 10_000 {
            return Err(Error::InvalidConfig(String::from("egfx grace period above 10 s")));
        }
        Ok(())
    }

    /// Effective encoder thread count.
    pub fn effective_encoder_threads(&self) -> u32 {
        if self.encoder_threads == 0 {
            2
        } else {
            self.encoder_threads
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServerConfig;

    #[test]
    fn default_is_valid() {
        ServerConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_bad_values() {
        let c = ServerConfig {
            max_fps: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_quantizer_ranges_outside_h264() {
        let c = ServerConfig {
            qp_max: 52,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
        let c = ServerConfig {
            qp_max: 0,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
        // A refinement frame that is coarser than the frame it replaces is a mistake,
        // not a preference: reject it rather than quietly encoding a worse picture.
        let c = ServerConfig {
            qp_max: 20,
            refine_qp: 30,
            ..ServerConfig::default()
        };
        assert!(c.validate().is_err());
        let c = ServerConfig {
            qp_max: 20,
            refine_qp: 20,
            ..ServerConfig::default()
        };
        c.validate().unwrap();
    }
}
