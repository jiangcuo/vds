//! Audio playback (MS-RDPEA, rdpsnd): 48 kHz, 16-bit, stereo PCM.
//!
//! The guest hands PCM over in whatever chunks QEMU's audio core produced, but the client is
//! fed waves of exactly [`PACK_MS`], taken from a ring that is allocated once.
//! Packing a guest chunk into waves allocates nothing; only the wave itself is copied out for
//! the event channel, which takes ownership of it.
//! Wave timestamps are counted from the samples themselves rather than read off the wall
//! clock, so a wave carries the moment it was *captured* however late it is sent — which is
//! what makes the client's Wave Confirm PDUs a usable playback position (MS-RDPEA 2.2.3.4).
//!
//! [`AudioState::backlog_ms`] is how much of what we handed the client it has not confirmed
//! playing yet — the client's queue, in other words — and past [`MAX_BACKLOG_MS`] the server
//! stops feeding it. The wave the client is behind on cannot be taken back: it
//! is already in the client's buffer. Withholding the new audio lets that buffer drain instead
//! of growing, and the samples not sent leave a gap in the media clock, which is the price of
//! catching up.
//!
//! Waves ready at the same moment are merged into one before they reach `ironrdp-server`, which
//! keeps only the newest four waves of a dispatch batch and drops older ones.
//!
//! Before a stream reaches the client, [`PREROLL_MS`] of it is held back and then sent at once,
//! so the client starts with that much queued. The guest's audio reaches libvds on QEMU's
//! 10 ms audio timer, which runs on the busy main loop and is late by up to tens of
//! milliseconds; a client that only ever holds the last 10 ms runs dry at each delay and
//! crackles. The cushion absorbs those delays on the server's side, whatever buffering the
//! client asks for. After [`REPRIME_AFTER`] without audio the client has played its queue out,
//! so the next audio is held back again.
//!
//! The backlog is measured against the newest wave *sent* rather than the newest audio
//! captured, so it always falls back to zero once the client has played what it holds.
//! Measured against the capture clock it could only grow while the server withholds audio —
//! the client has nothing left to confirm — and the stream would never restart.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError};
use ironrdp_server::{RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

use crate::client_info::CHANNEL_RDPSND;
use crate::session::SessionShared;

const PCM_48K_STEREO: AudioFormat = AudioFormat {
    format: WaveFormat::PCM,
    n_channels: 2,
    n_samples_per_sec: 48_000,
    n_avg_bytes_per_sec: 192_000,
    n_block_align: 4,
    bits_per_sample: 16,
    data: None,
};

/// Audio in one wave: small enough to keep the client's buffer shallow, large enough not to
/// send a PDU every couple of milliseconds.
const PACK_MS: u32 = 10;

/// Frames per millisecond at 48 kHz — the unit of the media clock.
const FRAMES_PER_MS: u64 = 48;

/// Frames in one wave.
const PACK_FRAMES: usize = PACK_MS as usize * FRAMES_PER_MS as usize;

/// Bytes per frame: two 16-bit channels.
const FRAME_BYTES: usize = 4;

/// Samples (`i16`) per frame, which is how the guest's PCM arrives.
const SAMPLES_PER_FRAME: usize = FRAME_BYTES / 2;

/// Bytes in one wave: 1 920, the 10 ms of 48 kHz stereo s16 the client expects.
const PACK_BYTES: usize = PACK_FRAMES * FRAME_BYTES;

/// Waves the ring holds. Nothing drains it but the session loop, so it only fills while that
/// loop is busy: 16 × 10 ms of slack.
const RING_PACKS: usize = 16;

/// Backlog past which audio is discarded rather than queued.
const MAX_BACKLOG_MS: u32 = 200;

/// Audio held back before a stream reaches the client, released at once: the cushion that
/// absorbs the late runs of QEMU's audio timer (measured at up to 25 ms).
const PREROLL_MS: u32 = 60;

/// Waves in the pre-roll.
const PREROLL_PACKS: usize = (PREROLL_MS / PACK_MS) as usize;

/// Silence after which the client is assumed to have played its queue out.
const REPRIME_AFTER: Duration = Duration::from_millis(100);

/// A media time in milliseconds, from a frame count.
///
/// The count starts at zero with each stream and wraps after 49 days; the wire carries 32 bits
/// and the client only echoes the low 16 (MS-RDPEA 2.2.3.4), so neither is a concern here.
fn ms(frames: u64) -> u32 {
    u32::try_from(frames / FRAMES_PER_MS).unwrap_or(u32::MAX)
}

/// The 32-bit media time nearest `near` whose low 16 bits are `ts`.
///
/// `wTimeStamp` is 16 bits and wraps every 65 s, so a confirm is placed on the media clock by
/// picking the candidate within ±32 s of now. A confirm delayed by more than half a window
/// would be placed wrongly; at that point it is also indistinguishable from one for audio
/// captured now, so nothing better is available.
fn extend(near: u32, ts: u16) -> u32 {
    let candidate = (near & !0xffff) | u32::from(ts);
    if candidate > near.saturating_add(0x8000) {
        candidate.wrapping_sub(0x1_0000)
    } else if candidate.saturating_add(0x8000) < near {
        candidate.wrapping_add(0x1_0000)
    } else {
        candidate
    }
}

/// A fixed ring of complete waves, allocated once so a steady stream does not grow the heap.
#[derive(Debug)]
struct Ring {
    buf: Vec<u8>,
    /// Media timestamp of each slot.
    ts: [u32; RING_PACKS],
    /// Slot holding the oldest wave.
    read: usize,
    /// Waves held.
    len: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            buf: vec![0; RING_PACKS * PACK_BYTES],
            ts: [0; RING_PACKS],
            read: 0,
            len: 0,
        }
    }
}

impl Ring {
    fn clear(&mut self) {
        self.read = 0;
        self.len = 0;
    }

    /// Stores `pack` as the newest wave, dropping the oldest if the ring is somehow full.
    fn push(&mut self, pack: &[u8], ts: u32) {
        debug_assert_eq!(pack.len(), PACK_BYTES);
        if self.len == RING_PACKS {
            self.pop_slot();
        }
        let write = (self.read + self.len) % RING_PACKS;
        self.buf[write * PACK_BYTES..][..PACK_BYTES].copy_from_slice(pack);
        self.ts[write] = ts;
        self.len += 1;
    }

    /// Removes the oldest wave, copied out of the ring for the client.
    ///
    /// The copy is what the event channel takes ownership of, so it is the one allocation a
    /// wave costs; everything before it reuses its buffers.
    fn take(&mut self) -> Option<(Vec<u8>, u32)> {
        if self.len == 0 {
            return None;
        }
        let data = self.buf[self.read * PACK_BYTES..][..PACK_BYTES].to_vec();
        let ts = self.pop_slot();
        Some((data, ts))
    }

    /// Advances past the oldest slot, returning its timestamp.
    fn pop_slot(&mut self) -> u32 {
        debug_assert!(self.len > 0, "the ring is empty");
        let ts = self.ts[self.read];
        self.read = (self.read + 1) % RING_PACKS;
        self.len -= 1;
        ts
    }
}

/// Playback state of a session.
#[derive(Debug)]
pub(crate) struct AudioState {
    ready: bool,
    /// Waves packed and waiting for the session loop.
    ring: Ring,
    /// PCM of the wave being filled, at most `PACK_BYTES`. Kept between calls (this is what
    /// makes a guest write that completes no wave allocate nothing).
    partial: Vec<u8>,
    /// Frames captured since the stream started, sent or discarded: the media clock.
    frames: u64,
    /// Media time of the newest wave handed to the client — what its confirms are measured
    /// against.
    sent_ms: u32,
    /// Frames discarded so far because the client was late.
    discarded: u64,
    /// Media time the client reports having finished playing.
    played_ms: u32,
    /// Whether a confirm has arrived: without one the client's position is unknown.
    confirmed: bool,
    /// The pre-roll is being collected: nothing goes out until [`PREROLL_PACKS`] waves wait.
    priming: bool,
    /// When guest audio last arrived.
    last_push: Option<Instant>,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            ready: false,
            ring: Ring::default(),
            partial: Vec::with_capacity(PACK_BYTES),
            frames: 0,
            sent_ms: 0,
            discarded: 0,
            played_ms: 0,
            confirmed: false,
            priming: true,
            last_push: None,
        }
    }
}

impl AudioState {
    fn ready(&self) -> bool {
        self.ready
    }

    /// Begins a stream: the clock, the queue and the client's position all start over.
    fn start(&mut self) {
        self.ring.clear();
        self.partial.clear();
        self.frames = 0;
        self.sent_ms = 0;
        self.discarded = 0;
        self.played_ms = 0;
        self.confirmed = false;
        self.priming = true;
        self.last_push = None;
        self.ready = true;
    }

    fn stop(&mut self) {
        self.ready = false;
    }

    /// How much of the audio the client was handed it has not confirmed playing, in
    /// milliseconds: the client's queue.
    ///
    /// Zero until a Wave Confirm arrives: with no confirm the client's position is unknown,
    /// and audio is never withheld on a guess.
    fn backlog_ms(&self) -> u32 {
        if !self.confirmed {
            return 0;
        }
        self.sent_ms.saturating_sub(self.played_ms)
    }

    /// Takes the client's playback position from a Wave Confirm.
    fn note_confirm(&mut self, client_ts: u16) {
        // The confirm is for a wave that was sent, so the newest one sent is what its 16 bits
        // have to be placed against. The client echoes the wave's timestamp plus the time it
        // held the data, so the value can sit just ahead of the wave it names but never ahead
        // of the newest wave sent.
        let played = extend(self.sent_ms, client_ts).min(self.sent_ms);
        if !self.confirmed || played > self.played_ms {
            self.played_ms = played;
        }
        self.confirmed = true;
        trace!(
            played_ms = played,
            backlog_ms = self.backlog_ms(),
            "client audio position"
        );
    }

    /// Hands one chunk of guest PCM to the stream.
    fn push(&mut self, pcm: &[i16]) {
        let now = Instant::now();
        if self.last_push.is_some_and(|at| now.duration_since(at) > REPRIME_AFTER) {
            // The guest went quiet long enough for the client to play everything out.
            self.priming = true;
        }
        self.last_push = Some(now);
        let mut rest = pcm;
        while !rest.is_empty() {
            if self.backlog_ms() > MAX_BACKLOG_MS {
                // The client is late: throw this away rather than queue audio that would only
                // arrive later still. The clock advances over it, so a wave always carries the
                // moment it was captured.
                self.discard(rest.len() / SAMPLES_PER_FRAME);
                return;
            }
            let room = PACK_FRAMES - self.partial.len() / FRAME_BYTES;
            let take = rest.len().min(room * SAMPLES_PER_FRAME);
            for sample in &rest[..take] {
                self.partial.extend_from_slice(&sample.to_le_bytes());
            }
            rest = &rest[take..];
            if self.partial.len() == PACK_BYTES {
                let ts = ms(self.frames);
                self.frames += PACK_FRAMES as u64;
                // Moves the buffer out and back so filling the next wave can reuse it.
                let pack = core::mem::take(&mut self.partial);
                self.ring.push(&pack, ts);
                self.partial = pack;
                self.partial.clear();
            }
        }
    }

    /// Takes the oldest packed wave, if one is waiting, to hand it to the client.
    fn take(&mut self) -> Option<(Vec<u8>, u32)> {
        if self.priming {
            if self.ring.len < PREROLL_PACKS {
                return None;
            }
            self.priming = false;
        }
        let (data, ts) = self.ring.take()?;
        self.sent_ms = ts;
        Some((data, ts))
    }

    /// Throws away everything queued plus `frames` of new audio, and moves the clock over it.
    fn discard(&mut self, frames: usize) {
        let frames = frames + self.partial.len() / FRAME_BYTES;
        if frames == 0 {
            return;
        }
        self.partial.clear();
        self.ring.clear();
        self.frames += frames as u64;
        self.discarded += frames as u64;
        debug!(
            frames,
            backlog_ms = self.backlog_ms(),
            discarded_total_ms = ms(self.discarded),
            "discarding late audio"
        );
    }
}

fn is_pcm_48k(f: &AudioFormat) -> bool {
    f.format == WaveFormat::PCM && f.n_channels == 2 && f.n_samples_per_sec == 48_000 && f.bits_per_sample == 16
}

/// Forwards guest samples to the client.
pub(crate) fn on_host_audio(shared: &SessionShared, pcm: &[i16]) {
    if pcm.is_empty() {
        return;
    }
    // Held across the loop: `send_event` writes to an unbounded channel and cannot block, so
    // there is nothing to wait for while holding it.
    let mut audio = shared.audio.lock();
    if !audio.ready() {
        return;
    }
    audio.push(pcm);
    // Everything ready now travels as one wave. `ironrdp-server` keeps only the newest four
    // waves of a dispatch batch and drops the rest (`WAVE_KEEP`, meant for waves of tens of
    // milliseconds); with 10 ms waves, a late run of the audio timer or the pre-roll would
    // otherwise lose its oldest audio — a gap the client plays as a click.
    let Some((mut data, ts)) = audio.take() else {
        return;
    };
    while let Some((more, _)) = audio.take() {
        data.extend_from_slice(&more);
    }
    if let Some(debug) = &shared.debug {
        debug.audio_wave(data.len() / SAMPLES_PER_FRAME, data.len(), ts);
    }
    let _ = shared.send_event(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
}

/// Forwards the guest volume to the client.
pub(crate) fn on_host_volume(shared: &SessionShared, left: u16, right: u16, mute: bool) {
    if !shared.audio.lock().ready() {
        return;
    }
    if let Some(debug) = &shared.debug {
        debug.audio_volume(left, right, mute);
    }
    let (left, right) = if mute { (0, 0) } else { (left, right) };
    let _ = shared.send_event(ServerEvent::Rdpsnd(RdpsndServerMessage::SetVolume { left, right }));
}

pub(crate) struct SoundFactory {
    shared: Arc<SessionShared>,
}

impl SoundFactory {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self { shared }
    }
}

impl ServerEventSender for SoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.shared.set_event_sender(sender);
    }
}

impl SoundServerFactory for SoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Handler {
            shared: Arc::clone(&self.shared),
        })
    }
}

struct Handler {
    shared: Arc<SessionShared>,
}

impl core::fmt::Debug for Handler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handler").field("sid", &self.shared.sid).finish()
    }
}

impl RdpsndServerHandler for Handler {
    fn get_formats(&self) -> &[AudioFormat] {
        core::slice::from_ref(&PCM_48K_STEREO)
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        let chosen = common.iter().find(|f| is_pcm_48k(f.format()));
        if let Some(_format) = chosen {
            // rdpsnd is a static channel: the client offering formats means it opened it.
            // Hooking the negotiation rather than `start` keeps the record true even
            // when the session never plays a sound.
            self.shared.client.channel_opened(CHANNEL_RDPSND);
            self.shared.client.write(&self.shared);
        } else {
            debug!(formats = common.len(), "client offers no 48 kHz PCM format");
        }
        chosen
    }

    fn start(&mut self, _format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        info!("audio playback started");
        self.shared.audio.lock().start();
        Ok(())
    }

    fn stop(&mut self) {
        debug!("audio playback stopped");
        self.shared.audio.lock().stop();
    }

    fn wave_confirm(&mut self, block_no: u8, timestamp: u16) {
        if let Some(debug) = &self.shared.debug {
            debug.audio_confirm(block_no, timestamp);
        }
        self.shared.audio.lock().note_confirm(timestamp);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioState, FRAMES_PER_MS, MAX_BACKLOG_MS, PACK_BYTES, PACK_FRAMES, PACK_MS, PCM_48K_STEREO, PREROLL_PACKS,
        RING_PACKS, extend, is_pcm_48k, ms,
    };

    /// `frames` frames of silence.
    fn pcm(frames: usize) -> Vec<i16> {
        vec![0; frames * 2]
    }

    /// A started stream whose pre-roll is already sent, so waves come out as they complete.
    fn started() -> AudioState {
        let mut audio = AudioState::default();
        audio.start();
        audio.priming = false;
        audio
    }

    /// A new stream holds its first waves back, then hands them over at once; afterwards each
    /// wave goes out as it completes.
    #[test]
    fn a_new_stream_starts_with_a_cushion() {
        let mut audio = AudioState::default();
        audio.start();
        for _ in 0..PREROLL_PACKS - 1 {
            audio.push(&pcm(PACK_FRAMES));
            assert!(audio.take().is_none(), "the pre-roll is still being collected");
        }
        audio.push(&pcm(PACK_FRAMES));
        let mut released = 0;
        while audio.take().is_some() {
            released += 1;
        }
        assert_eq!(released, PREROLL_PACKS, "the whole cushion goes out at once");
        audio.push(&pcm(PACK_FRAMES));
        assert!(audio.take().is_some(), "then every wave as it completes");
    }

    #[test]
    fn recognises_pcm_48k() {
        assert!(is_pcm_48k(&PCM_48K_STEREO));
        let mono = ironrdp_rdpsnd::pdu::AudioFormat {
            n_channels: 1,
            ..PCM_48K_STEREO
        };
        assert!(!is_pcm_48k(&mono));
    }

    /// A wave is 10 ms of 48 kHz stereo, so a chunk that does not fill one waits for the rest
    /// of it, and a big chunk becomes several.
    #[test]
    fn packs_the_stream_into_fixed_waves() {
        let mut audio = started();
        audio.push(&pcm(1));
        assert!(audio.take().is_none(), "one frame is not a wave");

        audio.push(&pcm(PACK_FRAMES - 1));
        let (data, ts) = audio.take().expect("the wave completed");
        assert_eq!(data.len(), PACK_BYTES);
        assert_eq!(ts, 0);
        assert!(audio.take().is_none());

        audio.push(&pcm(PACK_FRAMES * 4));
        let waves: Vec<(usize, u32)> = (0..4)
            .filter_map(|_| audio.take().map(|(d, ts)| (d.len(), ts)))
            .collect();
        assert_eq!(
            waves,
            vec![(PACK_BYTES, 10), (PACK_BYTES, 20), (PACK_BYTES, 30), (PACK_BYTES, 40)]
        );
    }

    /// The timestamp counts samples, so it advances by `PACK_MS` per wave whatever the chunk
    /// sizes were — never by however long the caller took to hand the audio over.
    #[test]
    fn timestamps_come_from_the_sample_count() {
        let mut audio = started();
        for chunk in [1, PACK_FRAMES - 1, 7, PACK_FRAMES * 2 - 7, PACK_FRAMES] {
            audio.push(&pcm(chunk));
        }
        let mut ts = Vec::new();
        while let Some((_, t)) = audio.take() {
            ts.push(t);
        }
        assert_eq!(ts, vec![0, 10, 20, 30]);
        assert_eq!(ms(PACK_FRAMES as u64), PACK_MS);
        assert_eq!(PACK_FRAMES as u64, PACK_MS as u64 * FRAMES_PER_MS);
    }

    /// The client's position is placed on the media clock even across the 16-bit wrap.
    #[test]
    fn confirms_are_placed_on_the_media_clock() {
        assert_eq!(extend(1_000, 1_000), 1_000);
        assert_eq!(extend(70_000, 1_000), 66_536, "the same low bits one window earlier");
        assert_eq!(extend(100_000, 1_000), 132_072, "the same low bits one window later");
        assert_eq!(
            extend(65_535, 0),
            65_536,
            "a confirm for audio captured just after the wrap"
        );
    }

    /// Audio is only withheld once a confirm shows the client trailing, and then the clock
    /// keeps moving so the loss is visible as a gap.
    #[test]
    fn discards_late_audio_only_when_the_client_trails() {
        let mut audio = started();
        assert_eq!(audio.backlog_ms(), 0, "no confirm: the client's position is unknown");

        // One second in, nothing confirmed yet: every wave reaches the client.
        assert_eq!(feed(&mut audio, 100), 100);
        assert_eq!(audio.sent_ms, 990, "the newest wave sent is the last one packed");
        assert_eq!(ms(audio.frames), 1_000, "and the next wave waits for the samples");

        // The client reports having played up to 300 ms — 690 ms of what it holds unplayed —
        // so the next second is thrown away instead of sent.
        audio.note_confirm(300);
        assert_eq!(audio.backlog_ms(), 690);
        assert_eq!(feed(&mut audio, 100), 0, "the late second never reached the client");
        assert_eq!(
            ms(audio.frames),
            2_000,
            "the clock kept moving over the discarded audio"
        );
        assert_eq!(audio.sent_ms, 990, "nothing was sent over it");
        assert_eq!(audio.discarded, 100 * PACK_FRAMES as u64);

        // Exactly at the threshold is not yet late; past it is.
        audio.note_confirm(790);
        assert_eq!(audio.backlog_ms(), MAX_BACKLOG_MS);
        assert_eq!(feed(&mut audio, 2), 1, "one wave at the threshold, then one too late");

        // The client plays the wave it was last sent: the backlog is gone, and because it is
        // measured against what was sent rather than what was captured, the stream restarts on
        // the current media time — a whole second past the moment the client last confirmed.
        audio.note_confirm(2_000);
        assert_eq!(audio.backlog_ms(), 0);
        assert_eq!(feed(&mut audio, 1), 1, "the stream resumes");
        assert_eq!(audio.sent_ms, 2_020, "on the media time of the audio captured now");
        assert!(
            audio.backlog_ms() < MAX_BACKLOG_MS,
            "only what has been sent since the confirm counts against the client"
        );
    }

    /// The queue is bounded: the ring keeps the newest waves and the oldest go.
    #[test]
    fn the_ring_is_bounded() {
        let mut audio = started();
        for _ in 0..RING_PACKS * 3 {
            audio.push(&pcm(PACK_FRAMES));
        }
        assert_eq!(audio.ring.len, RING_PACKS);
        assert_eq!(
            audio.take().map(|(_, ts)| ts),
            Some(RING_PACKS as u32 * 2 * PACK_MS),
            "the first wave still queued is the RING_PACKS-th from the end"
        );
    }

    /// Hands `waves` waves of PCM in, returning how many came out for the client.
    fn feed(audio: &mut AudioState, waves: usize) -> usize {
        let mut sent = 0;
        for _ in 0..waves {
            audio.push(&pcm(PACK_FRAMES));
            while audio.take().is_some() {
                sent += 1;
            }
        }
        sent
    }
}
