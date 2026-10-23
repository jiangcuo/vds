//! Interface implemented by protocol sessions to receive host-originated data.

use crate::event::HostClipboardEvent;

/// A connected, authenticated protocol session.
///
/// Methods are called from arbitrary host threads and must not block.
pub trait SessionSink: Send + Sync {
    /// Clipboard event from the guest.
    fn clipboard(&self, event: HostClipboardEvent);
    /// Audio samples from the guest: 48 kHz, signed 16-bit, stereo interleaved.
    fn audio_out(&self, pcm: &[i16]);
    /// Guest playback volume (0..=65535 per channel).
    fn audio_volume(&self, left: u16, right: u16, mute: bool);
    /// Closes the session.
    fn disconnect(&self, reason: &str);
}
