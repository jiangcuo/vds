//! EGFX (MS-RDPEGFX) with AVC420 frames encoded by openh264 (or VA-API), plus ClearCodec tiles for the areas
//! that stop changing.
//!
//! One video thread per session handles every head: it converts damaged macroblocks to I420 and
//! queues `StartFrame`/`WireToSurface1`/`EndFrame` through the IronRDP event loop. Each head's
//! H.264 encoder runs on its own thread with two picture buffers, so frame N+1 is converted
//! while frame N is encoded; frames still leave in order. Frame acknowledgements drive the pace (backpressure).
//!
//! With the mix on (`egfx-mixed`) the same pass marks the blocks that keep
//! changing as video ([`crate::blocks`]) and sends the rest of the damage as lossless
//! ClearCodec tiles in the same frame, so text and interface elements reach the client as exact
//! pixels. Everything the H.264 frame covers is an approximation until a tile replaces it,
//! which is what the block map's `lossy` flag tracks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::{
    Avc420Region, CapabilitiesAdvertisePdu, CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, MixedTilePayload};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc;
use tracing::{debug, info, info_span, warn};
use vds_core::codec::EgfxStatus;
use vds_core::config::ServerConfig;
use vds_core::debug::FrameRow;
use vds_core::geom::{Rect, Region};
use vds_core::config::VideoEncoderKind;
use vds_core::display::FrameRef;
use vds_encode::{
    BgrxFrame, EncodedFrame, EncoderConfig, I420Frame, OpenH264Encoder, RateControl, VaapiConfig, VaapiEncoder,
};

use crate::blocks::BlockMap;
use crate::clearcodec::TileEncoder;
use crate::client_info::CHANNEL_GFX;
use crate::session::SessionShared;

/// Frames in flight before the server waits for acknowledgements.
const MAX_FRAMES_IN_FLIGHT: u32 = 3;
/// Frames whose quantizer is above this leave their area marked for refinement.
const LOSSY_QP: u8 = 22;
/// How long the client's frame queue may stay full before the bitrate is lowered.
///
/// A queue that is full for a moment is normal — the client decodes on its GPU and
/// acknowledges a frame late. Answering that by spending fewer bits would blur a picture
/// that is merely one frame behind, which libvds rejects.
const BACKPRESSURE_GRACE: Duration = Duration::from_secs(3);
/// Initial bitrate cap when `bitrate=auto`.
const AUTO_START_KBPS: u32 = 20_000;
const AUTO_MAX_KBPS: u32 = 50_000;
/// Floor of the automatic cap. Below this the picture degrades rather than the frame rate.
const AUTO_MIN_KBPS: u32 = 5_000;
/// Rectangles a head remembers for refinement at once.
const LOSSY_MAX_RECTS: usize = 64;
/// Lossless tiles one frame carries at most, before its byte budget is even considered.
const MAX_TILES: usize = 16;

/// Whether a negotiated capability set enables AVC420.
fn avc420_enabled(cap: &CapabilitySet) -> bool {
    match cap {
        CapabilitySet::V8 { .. } => false,
        CapabilitySet::V8_1 { flags } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            !flags.contains(CapabilitiesV10Flags::AVC_DISABLED)
        }
        CapabilitySet::V10_1 => true,
        CapabilitySet::V10_3 { flags } => !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => !flags.contains(CapabilitiesV104Flags::AVC_DISABLED),
        CapabilitySet::V10_7 { flags } => !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
        #[expect(unreachable_patterns, reason = "CapabilitySet may gain variants")]
        _ => false,
    }
}

/// Server capability sets offered to the client.
///
/// IronRDP confirms `client_flags & server_flags`. `AVC_DISABLED` is a negative flag, so the
/// server offers every flag: the confirmed set then equals the client's own flags, and a
/// client that disabled AVC is not treated as AVC-capable.
fn server_capabilities() -> Vec<CapabilitySet> {
    vec![
        CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::all(),
        },
        CapabilitySet::V10_6 {
            flags: CapabilitiesV104Flags::all(),
        },
        CapabilitySet::V10_5 {
            flags: CapabilitiesV104Flags::all(),
        },
        CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::all(),
        },
        CapabilitySet::V10_3 {
            flags: CapabilitiesV103Flags::all(),
        },
        CapabilitySet::V10_2 {
            flags: CapabilitiesV10Flags::all(),
        },
        CapabilitySet::V10_1,
        CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::all(),
        },
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::all(),
        },
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::all(),
        },
    ]
}

pub(crate) struct GfxFactory {
    shared: Arc<SessionShared>,
}

impl GfxFactory {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self { shared }
    }
}

impl ServerEventSender for GfxFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.shared.set_event_sender(sender);
    }
}

impl GfxServerFactory for GfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GfxHandler {
            shared: Arc::clone(&self.shared),
        })
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = GraphicsPipelineServer::new(self.build_gfx_handler());
        let handle: GfxServerHandle = Arc::new(std::sync::Mutex::new(server));
        *self.shared.gfx.lock() = Some(Arc::clone(&handle));
        Some((GfxDvcBridge::new(Arc::clone(&handle)), handle))
    }
}

struct GfxHandler {
    shared: Arc<SessionShared>,
}

impl GraphicsPipelineHandler for GfxHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        debug!(sets = pdu.0.len(), "client advertised EGFX capabilities");
        self.shared
            .client
            .set_egfx_advertised(pdu.0.iter().map(|set| format!("{:?}", set.version)).collect());
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        let avc420 = avc420_enabled(negotiated);
        info!(caps = ?negotiated, avc420, "EGFX channel ready");
        self.shared.client.set_egfx_confirmed(format!("{negotiated:?}"), avc420);
        self.shared.set_egfx_status(EgfxStatus::Ready { avc420 });
        // Reached only if the client accepted the Graphics DVC and negotiated capabilities on
        // it.
        self.shared.client.channel_opened(CHANNEL_GFX);
        self.shared.client.write(&self.shared);
    }

    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32, _total_frames_decoded: u32) {
        self.shared.set_frames_in_flight(queue_depth);
        if let Some(debug) = &self.shared.debug {
            debug.frame_acked(frame_id);
        }
        // Wakes the video thread, which waits on the display signal.
        self.shared.core.display().signal().raise();
    }

    fn on_close(&mut self) {
        info!("EGFX channel closed");
        self.shared.set_egfx_status(EgfxStatus::Unavailable);
    }

    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        server_capabilities()
    }

    fn max_frames_in_flight(&self) -> u32 {
        MAX_FRAMES_IN_FLIGHT
    }
}

/// Handle of the video thread of a session.
pub(crate) struct VideoWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<SessionShared>,
}

impl VideoWorker {
    pub(crate) fn spawn(shared: Arc<SessionShared>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(format!("vds-video-{}", shared.sid))
                .spawn(move || {
                    let span = info_span!("encoder", sid = shared.sid);
                    let _guard = span.enter();
                    VideoLoop::new(shared, stop).run();
                })?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
            shared,
        })
    }

    /// Stops the thread and waits for it.
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        self.shared.core.display().signal().raise();
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            warn!("video thread panicked");
        }
    }
}

/// Areas whose most recent frame was encoded too coarsely, with the quantizer it used.
///
/// Only the last send of a rectangle matters: one that is sent again at a good quantizer
/// needs no refinement at all, so its entry is updated (or dropped) instead of kept.
#[derive(Debug, Default)]
struct LossyArea {
    rects: Vec<(Rect, u8)>,
}

impl LossyArea {
    /// Records the quantizer the frame covering `region` was just encoded with.
    fn record(&mut self, region: &Region, qp: u8) {
        if qp <= LOSSY_QP {
            // Sent sharply: whatever this frame covered no longer needs refining.
            self.rects
                .retain(|(r, _)| region.rects().iter().all(|d| d.intersect(r).area() == 0));
            return;
        }
        for r in region.rects() {
            match self.rects.iter_mut().find(|(k, _)| k == r) {
                Some(entry) => entry.1 = qp,
                None => {
                    if self.rects.len() == LOSSY_MAX_RECTS {
                        // Drop the oldest; a rectangle that is still bad is marked again
                        // by the next frame that touches it.
                        self.rects.remove(0);
                    }
                    self.rects.push((*r, qp));
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The area that is owed a refinement frame.
    fn region(&self) -> Region {
        let mut out = Region::new();
        for (r, _) in &self.rects {
            out.add(*r);
        }
        out
    }

    fn clear(&mut self) {
        self.rects.clear();
    }
}

struct HeadEncoder {
    index: u32,
    surface: u16,
    src_w: u32,
    src_h: u32,
    /// The H.264 encoder, on its own thread.
    worker: EncodeWorker,
    /// Picture buffers not at the encoder, each with the area it has not been converted with
    /// yet (what went into the other buffer since).
    free: Vec<(Picture, Region)>,
    /// The frame the encoder is working on: sent before the next one is submitted.
    in_flight: Option<InFlight>,
    need_idr: bool,
    next_frame_at: Instant,
    lossy: LossyArea,
    lossy_since: Option<Instant>,
    /// Which blocks the client has exactly and which it has as H.264 output.
    blocks: BlockMap,
    /// Encoder of the lossless tiles of this head.
    clear: TileEncoder,
}

/// Measured output and automatic bitrate adaptation.
struct RateState {
    auto: bool,
    cap_kbps: u32,
    fps: u32,
    qp_max: u8,
    window_start: Instant,
    frames: u32,
    bytes: u64,
    /// The client's frame queue was full during this window.
    backpressured: bool,
    /// When the current run of backpressure started.
    backpressure_since: Option<Instant>,
}

impl RateState {
    fn new(config: &ServerConfig) -> Self {
        Self {
            auto: config.bitrate_kbps == 0,
            cap_kbps: if config.bitrate_kbps == 0 {
                AUTO_START_KBPS
            } else {
                config.bitrate_kbps
            },
            fps: config.max_fps,
            qp_max: config.qp_max,
            window_start: Instant::now(),
            frames: 0,
            bytes: 0,
            backpressured: false,
            backpressure_since: None,
        }
    }

    fn rate(&self) -> RateControl {
        RateControl::capped(self.cap_kbps, self.fps, self.qp_max)
    }

    /// The client is not acknowledging fast enough to keep the queue draining.
    fn note_backpressure(&mut self) {
        self.backpressured = true;
        self.backpressure_since.get_or_insert_with(Instant::now);
    }
}

struct VideoLoop {
    shared: Arc<SessionShared>,
    stop: Arc<AtomicBool>,
    heads: Vec<HeadEncoder>,
    layout_generation: Option<u64>,
    interval: Duration,
    refine_delay: Option<Duration>,
    refine_qp: u8,
    /// Whether still areas travel as ClearCodec tiles beside the H.264 frames.
    mixed: bool,
    /// How long an area must be quiet before its lossless re-send, in mixed mode.
    settle: Duration,
    rate: RateState,
}

/// What a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// Fresh damage: an H.264 frame, with the still part of it as tiles when the mix is on.
    Damage,
    /// Areas last sent coarsely, re-encoded (without the mix).
    Refine,
    /// Lossless tiles alone: a picture the client has as H.264 output, made exact.
    Tiles,
}

/// Taking the frame: what the H.264 frame covers and the lossless tiles taken with it.
#[derive(Debug, Default)]
struct Taken {
    /// What the H.264 frame covers, empty when the frame carries no picture.
    avc: Region,
    /// Lossless tiles: block-aligned rectangle and encoded ClearCodec stream.
    tiles: Vec<(Rect, Vec<u8>)>,
}

/// Picture buffers per head: one at the encoder while the video thread converts into the other.
const PICTURE_BUFFERS: usize = 2;

/// A picture buffer: what the encoder of the head takes.
enum Picture {
    /// openh264: converted to I420 on the CPU.
    I420(I420Frame),
    /// VA-API: BGRX as the host has it; the GPU converts it.
    Bgrx(BgrxFrame),
}

impl Picture {
    /// A black `width`x`height` picture for `encoder`.
    fn new(encoder: &Encoder, width: u32, height: u32) -> vds_encode::Result<Self> {
        Ok(match encoder {
            Encoder::OpenH264(_) => Self::I420(I420Frame::new(width, height)?),
            Encoder::Vaapi(_) => Self::Bgrx(BgrxFrame::new(width, height)?),
        })
    }

    /// Brings `rect` up to date from `frame`: a colour conversion for openh264, a copy for VA-API.
    fn update(&mut self, frame: &FrameRef<'_>, rect: Rect) -> vds_encode::Result<()> {
        match self {
            Self::I420(p) => p.convert(frame, rect),
            Self::Bgrx(p) => p.copy_rect(frame, rect),
        }
    }
}

/// A head's H.264 encoder.
enum Encoder {
    // openh264 keeps its whole parameter set inline (7 KiB): boxed, so the enum stays small.
    OpenH264(Box<OpenH264Encoder>),
    Vaapi(VaapiEncoder),
}

impl Encoder {
    fn encode(
        &mut self,
        picture: &Picture,
        idr: bool,
        qp: Option<u8>,
        pts_ms: i64,
    ) -> vds_encode::Result<Option<EncodedFrame>> {
        match (self, picture) {
            (Self::OpenH264(e), Picture::I420(p)) => e.encode(p, idr, qp, pts_ms),
            // The hardware takes no per-frame quantizer: a refinement frame is an ordinary one.
            (Self::Vaapi(e), Picture::Bgrx(p)) => e.encode(p, idr, pts_ms),
            _ => Err(vds_encode::Error::Encode),
        }
    }

    fn set_rate(&mut self, rate: RateControl) -> vds_encode::Result<()> {
        match self {
            Self::OpenH264(e) => e.set_rate(rate),
            Self::Vaapi(e) => e.set_rate(rate.vbv_max_kbps),
        }
    }
}

/// Work for a head's encoder thread.
enum EncodeJob {
    Encode {
        picture: Picture,
        idr: bool,
        qp: Option<u8>,
        pts_ms: i64,
    },
    Rate(RateControl),
}

/// An encoded picture, with the buffer it came from.
struct EncodeDone {
    picture: Picture,
    result: vds_encode::Result<Option<EncodedFrame>>,
    encode_us: u64,
}

/// A head's H.264 encoder on its own thread.
///
/// The video thread takes and converts frame N+1 while this thread encodes frame N; doing both
/// in turn on one thread made the conversion and the encode add up and capped the frame rate.
/// Frames still leave in order: frame N is sent before frame N+1 is submitted.
struct EncodeWorker {
    jobs: Option<std::sync::mpsc::Sender<EncodeJob>>,
    done: std::sync::mpsc::Receiver<EncodeDone>,
    thread: Option<JoinHandle<()>>,
}

impl EncodeWorker {
    /// Starts the thread; `wake` is raised whenever a picture is encoded.
    fn spawn(mut encoder: Encoder, head: u32, wake: vds_core::Core) -> std::io::Result<Self> {
        let (jobs, job_rx) = std::sync::mpsc::channel::<EncodeJob>();
        let (done_tx, done) = std::sync::mpsc::channel::<EncodeDone>();
        let thread = std::thread::Builder::new()
            .name(format!("vds-h264-{head}"))
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    match job {
                        EncodeJob::Encode {
                            picture,
                            idr,
                            qp,
                            pts_ms,
                        } => {
                            let started = Instant::now();
                            let result = encoder.encode(&picture, idr, qp, pts_ms);
                            let encode_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                            if done_tx
                                .send(EncodeDone {
                                    picture,
                                    result,
                                    encode_us,
                                })
                                .is_err()
                            {
                                break;
                            }
                            wake.display().signal().raise();
                        }
                        EncodeJob::Rate(rate) => {
                            if let Err(e) = encoder.set_rate(rate) {
                                vds_core::warn_ratelimited!(error = %e, "failed to change the bitrate");
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            done,
            thread: Some(thread),
        })
    }

    /// Queues `job`; gives it back if the thread is gone.
    fn submit(&self, job: EncodeJob) -> Result<(), Box<EncodeJob>> {
        match &self.jobs {
            Some(jobs) => jobs.send(job).map_err(|e| Box::new(e.0)),
            None => Err(Box::new(job)),
        }
    }

    /// The next encoded picture; `None` if the thread is gone.
    fn wait(&self) -> Option<EncodeDone> {
        self.done.recv().ok()
    }

    /// The next encoded picture if it is ready.
    fn try_done(&self) -> Option<EncodeDone> {
        self.done.try_recv().ok()
    }
}

impl Drop for EncodeWorker {
    fn drop(&mut self) {
        // Closing the job queue ends the thread once its current picture is done.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A frame taken from the head and converted, ready for the encoder (or tiles alone).
struct Prepared {
    taken: Taken,
    refine: bool,
    qp: Option<u8>,
    ts: u32,
    convert_us: u64,
    tile_us: u64,
    age_us: u64,
}

/// A frame at the encoder.
struct InFlight {
    prepared: Prepared,
    /// What its buffer misses: the damage converted into the other buffer meanwhile.
    missing: Region,
}

/// What the EGFX server queued for one encode, and in which frames.
#[derive(Debug, Default)]
struct Queued {
    /// Frame id of the H.264 frame, when one was queued.
    avc: Option<u32>,
    /// Tiles that travelled inside that same frame.
    tiles_with_avc: usize,
    /// Frame id of a lossless-only frame, when one was queued separately.
    tiles: Option<u32>,
}

impl Queued {
    fn any(&self) -> bool {
        self.avc.is_some() || self.tiles.is_some()
    }
}

/// The wire form of a rectangle: `right` and `bottom` are exclusive, like the EGFX PDUs'.
fn exclusive(r: Rect) -> Option<ExclusiveRectangle> {
    Some(ExclusiveRectangle {
        left: u16::try_from(r.x).ok()?,
        top: u16::try_from(r.y).ok()?,
        right: u16::try_from(r.right()).ok()?,
        bottom: u16::try_from(r.bottom()).ok()?,
    })
}
impl VideoLoop {
    fn new(shared: Arc<SessionShared>, stop: Arc<AtomicBool>) -> Self {
        let config = shared.core.config();
        let interval = Duration::from_millis(1000 / u64::from(config.max_fps.max(1)));
        let mixed = config.egfx_mixed;
        // The settle time is `refine-delay` in both modes: how long an area has to stop
        // changing before the client is given its exact pixels. Without the mix a zero turns
        // the refinement frame off, with it zero means "as soon as it stops changing".
        let settle = Duration::from_millis(u64::from(config.refine_delay_ms));
        let refine_delay = (!mixed && config.refine_delay_ms > 0).then_some(settle);
        let refine_qp = config.refine_qp;
        let rate = RateState::new(config);
        Self {
            shared,
            stop,
            heads: Vec::new(),
            layout_generation: None,
            interval,
            refine_delay,
            refine_qp,
            mixed,
            settle,
            rate,
        }
    }

    /// Bytes a frame's lossless tiles may take: that frame's share of the bitrate cap.
    ///
    /// The H.264 frame and its tiles are one frame on the wire, and a frame's share of the cap
    /// is what the link carries without a queue building up. The first tile is sent whatever
    /// it costs (one tile is at most `blocks::MAX_TILE_PIXELS` pixels, so the overshoot is
    /// bounded); the budget decides how many follow it. An area left over stays marked and
    /// comes back in a later frame, once the tiles before it are exact.
    fn tile_budget(&self) -> u64 {
        u64::from(self.rate.cap_kbps) * 1000 / 8 / u64::from(self.rate.fps.max(1))
    }

    fn with_gfx<R>(&self, f: impl FnOnce(&mut GraphicsPipelineServer) -> R) -> Option<R> {
        let handle = self.shared.gfx.lock().clone()?;
        let mut guard = handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(f(&mut guard))
    }

    /// Encodes queued EGFX PDUs for the DVC and hands them to the IronRDP event loop.
    fn flush(&self, messages: Vec<ironrdp_dvc::DvcMessage>, channel_id: Option<u32>) -> bool {
        if messages.is_empty() {
            return true;
        }
        let Some(channel_id) = channel_id else {
            return false;
        };
        match encode_dvc_messages(channel_id, messages, ChannelFlags::SHOW_PROTOCOL) {
            Ok(messages) => self
                .shared
                .send_event(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages })),
            Err(e) => {
                warn!(error = %e, "failed to encode EGFX messages");
                false
            }
        }
    }

    fn run(mut self) {
        let core = self.shared.core.clone();
        let display = core.display();
        debug!("video thread started");
        while !self.stop.load(Ordering::Acquire) {
            let Some((ready, busy)) = self.with_gfx(|g| (g.is_ready() && g.supports_avc420(), g.should_backpressure()))
            else {
                break;
            };
            if !ready {
                display.set_busy(true);
                display.signal().wait_timeout(Duration::from_millis(100));
                continue;
            }

            let generation = display.layout_generation();
            if self.layout_generation != Some(generation) {
                match self.reconfigure() {
                    Ok(()) => self.layout_generation = Some(generation),
                    Err(e) => {
                        vds_core::warn_ratelimited!(error = %e, "failed to configure the video pipeline");
                        display.signal().wait_timeout(Duration::from_millis(200));
                        continue;
                    }
                }
            }

            display.set_busy(busy);
            if busy {
                self.rate.note_backpressure();
                // A picture the encoder finished is already paid for: send it rather than
                // let it go stale while the client catches up.
                for i in 0..self.heads.len() {
                    self.finish_in_flight(i, false);
                }
                self.tick();
                display.signal().wait_timeout(Duration::from_millis(50));
                continue;
            }

            let next = self.encode_due_heads();
            self.tick();
            let now = Instant::now();
            let timeout = next
                .map_or(Duration::from_secs(1), |t| t.saturating_duration_since(now))
                .min(Duration::from_secs(1));
            if !timeout.is_zero() {
                display.signal().wait_timeout(timeout);
            }
        }
        display.set_busy(false);
        debug!("video thread stopped");
    }

    /// Recreates surfaces and encoders for the current head layout.
    fn reconfigure(&mut self) -> Result<(), String> {
        let display = self.shared.core.display();
        let layout = display.layout();
        let config = self.shared.core.config();
        let settle_ms = u32::try_from(self.settle.as_millis()).unwrap_or(u32::MAX);
        let refresh_ms = u32::try_from(self.interval.as_millis()).unwrap_or(u32::MAX);
        let mut heads = Vec::with_capacity(layout.len());
        // The old encoder threads are stopped before new ones start.
        self.heads.clear();
        for l in &layout {
            let encoder = self.open_encoder(config, l.width, l.height)?;
            let mut free = Vec::with_capacity(PICTURE_BUFFERS);
            for _ in 0..PICTURE_BUFFERS {
                let picture = Picture::new(&encoder, l.width, l.height).map_err(|e| e.to_string())?;
                // A fresh buffer is black: it misses everything until the first full frame.
                free.push((picture, Region::from_rect(Rect::new(0, 0, l.width, l.height))));
            }
            let worker = EncodeWorker::spawn(encoder, l.index, self.shared.core.clone())
                .map_err(|e| format!("cannot start the encoder thread: {e}"))?;
            heads.push(HeadEncoder {
                index: l.index,
                surface: 0,
                src_w: l.width,
                src_h: l.height,
                worker,
                free,
                in_flight: None,
                need_idr: true,
                next_frame_at: Instant::now(),
                lossy: LossyArea::default(),
                lossy_since: None,
                blocks: BlockMap::new(l.width, l.height, settle_ms, refresh_ms),
                clear: TileEncoder::new(),
            });
        }

        let (dw, dh) = display.desktop_size();
        let monitors: Vec<Monitor> = layout
            .iter()
            .enumerate()
            .map(|(i, l)| Monitor {
                left: i32::try_from(l.x).unwrap_or(i32::MAX),
                top: i32::try_from(l.y).unwrap_or(i32::MAX),
                right: i32::try_from(l.x + l.width - 1).unwrap_or(i32::MAX),
                bottom: i32::try_from(l.y + l.height - 1).unwrap_or(i32::MAX),
                flags: if i == 0 {
                    MonitorFlags::PRIMARY
                } else {
                    MonitorFlags::empty()
                },
            })
            .collect();
        let (messages, channel_id) = self
            .with_gfx(|g| -> Result<_, String> {
                let w = u16::try_from(dw.max(1)).map_err(|_| String::from("desktop too wide"))?;
                let h = u16::try_from(dh.max(1)).map_err(|_| String::from("desktop too tall"))?;
                g.resize_with_monitors(w, h, monitors);
                for (he, l) in heads.iter_mut().zip(&layout) {
                    // A surface as large as the display, not as the padded picture: the
                    // client draws the picture 1:1 (MS-RDPEGFX 2.2.4.4), any mismatch would
                    // be scaled.
                    let sw = u16::try_from(he.src_w).map_err(|_| String::from("surface width"))?;
                    let sh = u16::try_from(he.src_h).map_err(|_| String::from("surface height"))?;
                    he.surface = g.create_surface(sw, sh).ok_or("surface creation refused")?;
                    g.map_surface_to_output(he.surface, l.x, l.y);
                }
                Ok((g.drain_output(), g.channel_id()))
            })
            .ok_or_else(|| String::from("EGFX channel gone"))??;
        if !self.flush(messages, channel_id) {
            return Err(String::from("session event queue closed"));
        }
        info!(heads = heads.len(), w = dw, h = dh, "EGFX surfaces configured");
        self.heads = heads;
        display.invalidate_all();
        Ok(())
    }

    /// Opens the configured encoder for a `width`x`height` head; VA-API falls back to openh264.
    fn open_encoder(&self, config: &ServerConfig, width: u32, height: u32) -> Result<Encoder, String> {
        if config.encoder == VideoEncoderKind::Vaapi {
            let vaapi = VaapiEncoder::open(&VaapiConfig {
                width,
                height,
                fps: config.max_fps,
                render_node: config.render_node.clone(),
                bitrate_kbps: self.rate.cap_kbps,
            });
            match vaapi {
                Ok(e) => {
                    info!(width, height, "H.264 on the GPU (VA-API)");
                    return Ok(Encoder::Vaapi(e));
                }
                Err(e) => warn!(error = %e, "VA-API encoder unavailable, using openh264"),
            }
        }
        OpenH264Encoder::open(&EncoderConfig {
            // The source size, not the padded picture: the encoder signals the padding as SPS
            // cropping so the client decodes at the display resolution.
            width,
            height,
            fps: config.max_fps,
            threads: config.effective_encoder_threads(),
            rate: self.rate.rate(),
        })
        .map(|e| Encoder::OpenH264(Box::new(e)))
        .map_err(|e| e.to_string())
    }

    /// Encodes every head that is due. Returns the next time something becomes due.
    fn encode_due_heads(&mut self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut earliest = |t: Instant| {
            next = Some(next.map_or(t, |n| n.min(t)));
        };
        for i in 0..self.heads.len() {
            // A picture the encoder has finished goes out now; the encoder thread wakes the
            // loop when it is done.
            self.finish_in_flight(i, false);
            let now = Instant::now();
            let Some(head) = self.shared.core.display().head(self.heads[i].index) else {
                continue;
            };
            let he = &self.heads[i];
            let damaged = he.need_idr || head.has_damage();
            // A mixed frame also carries tiles when nothing is damaged at all: that is the
            // frame that makes an area the client has as H.264 output exact again.
            let owed = self.mixed && he.blocks.still_due(now);
            let refine_at = match (self.refine_delay, he.lossy_since) {
                (Some(delay), Some(since)) => Some(since + delay),
                _ => None,
            };
            let kind = if damaged {
                FrameKind::Damage
            } else if owed {
                FrameKind::Tiles
            } else if refine_at.is_some_and(|at| now >= at) {
                FrameKind::Refine
            } else {
                if let Some(at) = refine_at {
                    earliest(at);
                }
                continue;
            };
            if now < he.next_frame_at {
                earliest(he.next_frame_at);
                continue;
            }
            if self.encode_head(i, kind) {
                self.heads[i].next_frame_at = now + self.interval;
            }
            // Re-check backpressure before encoding another head.
            return Some(Instant::now());
        }
        next
    }

    /// Takes, converts and submits one frame of head `i`, and sends the frame the encoder
    /// finished before it. Returns `true` if a frame was submitted or sent.
    fn encode_head(&mut self, i: usize, kind: FrameKind) -> bool {
        // Convert first: the encoder may still be busy with the previous frame meanwhile.
        let Some((prepared, picture)) = self.prepare(i, kind) else {
            return false;
        };
        // Frames leave in order: whatever the encoder holds goes out before this one.
        let mut sent = self.finish_in_flight(i, true);
        let Some(picture) = picture else {
            // Lossless tiles alone: nothing for the H.264 encoder.
            return self.send_frame(i, prepared, None) || sent;
        };
        let he = &mut self.heads[i];
        // Decided now, not when the frame was taken: a frame sent just before may have failed.
        let idr = he.need_idr;
        let job = EncodeJob::Encode {
            picture,
            idr,
            qp: prepared.qp,
            pts_ms: i64::from(prepared.ts),
        };
        match he.worker.submit(job) {
            Ok(()) => {
                he.in_flight = Some(InFlight {
                    prepared,
                    missing: Region::new(),
                });
                sent = true;
            }
            Err(job) => {
                vds_core::warn_ratelimited!(head = he.index, "encoder thread gone, requesting idr");
                he.need_idr = true;
                if let EncodeJob::Encode { picture, .. } = *job {
                    // The buffer was converted for a frame that never went out: everything
                    // in it is stale for the encoder's next picture.
                    he.free
                        .push((picture, Region::from_rect(Rect::new(0, 0, he.src_w, he.src_h))));
                }
            }
        }
        sent
    }

    /// Takes the frame of head `i` and converts what it needs into a free picture buffer.
    ///
    /// Returns the frame and, when it carries H.264, the buffer to encode. `None` when there is
    /// nothing to send.
    fn prepare(&mut self, i: usize, kind: FrameKind) -> Option<(Prepared, Option<Picture>)> {
        let head = self.shared.core.display().head(self.heads[i].index)?;
        // ClearCodec tiles are numbered in the order the client receives them, and a frame's
        // numbers are only committed once it is sent: tiles are taken only when no frame is at
        // the encoder. When tiles are owed, the frame there goes out first.
        if self.mixed && self.heads[i].in_flight.is_some() && self.heads[i].blocks.still_due(Instant::now()) {
            self.finish_in_flight(i, true);
        }
        let tiles_allowed = self.heads[i].in_flight.is_none();
        // Read before the head is borrowed: both borrow the whole loop.
        let (mixed, budget) = (self.mixed, self.tile_budget());
        let ts = self.shared.timestamp_ms();
        let refine_qp = self.refine_qp;
        let he = &mut self.heads[i];
        // Damage and tiles are limited to the display size: the encode padding is cropped away
        // by the SPS, so the client has no surface area for it.
        let bounds = Rect::new(0, 0, he.src_w, he.src_h);
        let refine = kind == FrameKind::Refine;
        let Some((mut picture, missing)) = he.free.pop() else {
            // Both buffers are out: cannot happen with one frame at the encoder at a time.
            return None;
        };

        let mut convert_us = 0_u64;
        let mut tile_us = 0_u64;
        // How long the picture has been stale when the frame is started. A refinement
        // frame carries no fresh damage, so it has no age.
        let mut age_us = 0_u64;
        let converted = if refine {
            // Without the mix: a refinement frame re-encodes what is still marked lossy.
            // The buffer only needs what went into the other one meanwhile.
            let converting = Instant::now();
            let ok = head.with_frame(|frame| {
                frame.width == he.src_w
                    && frame.height == he.src_h
                    && missing.rects().iter().all(|r| picture.update(&frame, *r).is_ok())
            });
            convert_us = u64::try_from(converting.elapsed().as_micros()).unwrap_or(u64::MAX);
            ok.then(|| Taken {
                avc: he.lossy.region(),
                tiles: Vec::new(),
            })
        } else {
            let now_taken = Instant::now();
            let (src_w, src_h, need_idr) = (he.src_w, he.src_h, he.need_idr);
            head.take_frame(|frame, damage, age| {
                if frame.width != src_w || frame.height != src_h {
                    // A resize is pending; the layout change reconfigures everything.
                    return None;
                }
                age_us = age.map_or(0, |a| u64::try_from(a.as_micros()).unwrap_or(u64::MAX));
                // What the host changed drives the classification, an IDR or not: resending the
                // whole screen says nothing about which blocks are the ones that keep changing.
                he.blocks.observe(&damage, now_taken);
                let damage = if need_idr {
                    Region::from_rect(Rect::new(0, 0, src_w, src_h))
                } else {
                    damage
                };

                let mut tiles = Vec::new();
                if mixed && tiles_allowed {
                    let tiling = Instant::now();
                    let mut still_bytes = 0_usize;
                    // The number the next tile that survives the budget check carries. The
                    // encoder itself only advances once the frame is on the wire, so a tile
                    // dropped here does not leave a hole the client would reject.
                    let mut tile_seq = he.clear.next_seq();
                    for rect in he.blocks.plan(now_taken, MAX_TILES) {
                        let rect = rect.intersect(&bounds);
                        if rect.is_empty() {
                            continue;
                        }
                        let data = he.clear.encode(frame.pixels, frame.stride, rect, tile_seq);
                        // The first tile always goes out — one tile is bounded by
                        // `blocks::MAX_TILE_PIXELS`, so the overshoot is bounded too — and the
                        // budget decides how many follow it. What is left over stays marked and
                        // comes back in a later frame.
                        if !tiles.is_empty() && still_bytes + data.len() > budget as usize {
                            break;
                        }
                        tile_seq = tile_seq.wrapping_add(1);
                        still_bytes += data.len();
                        tiles.push((rect, data));
                    }
                    tile_us = u64::try_from(tiling.elapsed().as_micros()).unwrap_or(u64::MAX);
                }

                // The H.264 frame carries what the tiles leave over. A damaged block no tile
                // covers has to travel here even when it is classified as still: its tile comes
                // in a later frame, once there is room for it, and until then nothing else would
                // tell the client it changed.
                let avc = if need_idr {
                    damage
                } else {
                    let rects: Vec<Rect> = tiles.iter().map(|(r, _)| *r).collect();
                    let mut avc = he.blocks.avc_regions(&damage, &rects);
                    // The grid's last row and column round up past the head; a region that
                    // reaches outside the surface is refused or drawn wrong.
                    avc.clip(&bounds);
                    avc
                };
                if !avc.is_empty() {
                    // The buffer also takes what went into the other one since it was last
                    // encoded, so the encoder always sees the whole current picture.
                    let mut convert = avc.clone();
                    convert.add_region(&missing);
                    convert.clip(&bounds);
                    let converting = Instant::now();
                    for r in convert.rects() {
                        if let Err(e) = picture.update(&frame, *r) {
                            vds_core::warn_ratelimited!(error = %e, "colour conversion failed");
                            return None;
                        }
                    }
                    convert_us = u64::try_from(converting.elapsed().as_micros()).unwrap_or(u64::MAX);
                }
                Some(Taken { avc, tiles })
            })
        };
        let Some(taken) = converted else {
            // The damage was consumed and the buffer may be half converted: resend everything
            // once possible.
            he.need_idr = true;
            he.free.push((picture, Region::from_rect(bounds)));
            return None;
        };
        let picture = if taken.avc.is_empty() {
            // Not converted into: the buffer keeps what it missed.
            he.free.push((picture, missing));
            if taken.tiles.is_empty() {
                return None;
            }
            None
        } else {
            // What this buffer now has, the other buffers (free or at the encoder) do not.
            for (_, other) in &mut he.free {
                other.add_region(&taken.avc);
            }
            if let Some(f) = &mut he.in_flight {
                f.missing.add_region(&taken.avc);
            }
            Some(picture)
        };
        let prepared = Prepared {
            taken,
            refine,
            qp: refine.then_some(refine_qp),
            ts,
            convert_us,
            tile_us,
            age_us,
        };
        Some((prepared, picture))
    }

    /// Sends the frame head `i`'s encoder is working on, waiting for it when `block`.
    /// Returns `true` if it was sent.
    fn finish_in_flight(&mut self, i: usize, block: bool) -> bool {
        let he = &mut self.heads[i];
        if he.in_flight.is_none() {
            return false;
        }
        let done = if block { he.worker.wait() } else { he.worker.try_done() };
        let Some(done) = done else {
            if block {
                // The encoder thread is gone with the picture.
                he.in_flight = None;
                he.need_idr = true;
            }
            return false;
        };
        let Some(flight) = he.in_flight.take() else {
            return false;
        };
        he.free.push((done.picture, flight.missing));
        self.send_frame(i, flight.prepared, Some((done.result, done.encode_us)))
    }

    /// Builds and queues the EGFX frame of head `i`: the encoded picture, if any, and the
    /// lossless tiles. Returns `true` if the frame was sent.
    fn send_frame(
        &mut self,
        i: usize,
        prepared: Prepared,
        encoded: Option<(vds_encode::Result<Option<EncodedFrame>>, u64)>,
    ) -> bool {
        let mixed = self.mixed;
        let Prepared {
            taken,
            refine,
            qp,
            ts,
            convert_us,
            tile_us,
            age_us,
        } = prepared;
        let he = &mut self.heads[i];
        let bounds = Rect::new(0, 0, he.src_w, he.src_h);
        let mut encode_us = tile_us;
        let mut frame_qp = 0_u8;
        let mut regions_src = Region::new();
        let mut regions: Vec<Avc420Region> = Vec::new();
        let mut kind_name = "clear";
        let h264 = match encoded {
            // Tiles alone: there is nothing for the H.264 encoder to do, and nothing that would invalidate
            // its reference frame, so a tile-only frame needs no IDR either.
            None => None,
            Some((result, us)) => {
                encode_us += us;
                let encoded = match result {
                    Ok(Some(f)) => f,
                    Ok(None) => return false,
                    Err(e) => {
                        vds_core::warn_ratelimited!(error = %e, head = he.index, "H.264 encode failed, requesting idr");
                        he.need_idr = true;
                        return false;
                    }
                };
                frame_qp = qp.unwrap_or_else(|| {
                    // `quality` is clamped to 0..=51 by the encoder.
                    u8::try_from(encoded.quality.round() as i64).unwrap_or(51)
                });
                regions_src = if encoded.idr {
                    Region::from_rect(bounds)
                } else {
                    taken.avc.clone()
                };
                let quality = u8::try_from(100 - u32::from(frame_qp.min(51)) * 100 / 51).unwrap_or(0);
                regions = regions_src
                    .rects()
                    .iter()
                    .filter_map(|r| {
                        // Right and bottom are exclusive, like the PDU's own rectangles (that is
                        // what `Avc420Region::full_frame` writes and what the client copies).
                        Some(Avc420Region {
                            left: u16::try_from(r.x).ok()?,
                            top: u16::try_from(r.y).ok()?,
                            right: u16::try_from(r.right()).ok()?,
                            bottom: u16::try_from(r.bottom()).ok()?,
                            quantization_parameter: frame_qp,
                            quality,
                        })
                    })
                    .collect();
                kind_name = if encoded.idr {
                    "idr"
                } else if refine {
                    "refine"
                } else {
                    "p"
                };
                Some(encoded.data)
            }
        };
        let region_count = regions.len();
        let region_area: u64 = regions_src
            .rects()
            .iter()
            .map(|r| u64::from(r.w) * u64::from(r.h))
            .sum();

        // The lossless tiles of this frame, in the order the blocks were planned.
        let mut tile_rects: Vec<Rect> = Vec::with_capacity(taken.tiles.len());
        let mut tile_bytes: Vec<usize> = Vec::with_capacity(taken.tiles.len());
        let mut payloads: Vec<MixedTilePayload> = Vec::with_capacity(taken.tiles.len());
        for (rect, data) in taken.tiles {
            // A tile is clipped to the head, whose size fits the `u16` its surface was created
            // with, so a tile's destination always fits the wire.
            let Some(destination) = exclusive(rect) else {
                continue;
            };
            tile_rects.push(rect);
            tile_bytes.push(data.len());
            payloads.push(MixedTilePayload::ClearCodec {
                destination,
                bitmap_data: data,
            });
        }

        let surface = he.surface;
        let h264_len = h264.as_ref().map_or(0, Vec::len);
        let record_video = self.shared.debug.is_some() && vds_core::debug::video_enabled();
        let video_stream = record_video.then(|| h264.clone()).flatten();
        let mixed_ok = self
            .with_gfx(|g| g.codec_capabilities().avc420_in_mixed_frames)
            .unwrap_or(false);
        // Encoding is done; what is left is handing the frame over — building the EGFX
        // PDUs, serialising them and pushing them into the session's event loop. Everything
        // after that (the socket write, the client's decode) is only visible as `ack_us`.
        let handoff_started = Instant::now();
        let sent = self.with_gfx(move |g| {
            let mut q = Queued::default();
            let tiles = payloads;
            let n_tiles = tiles.len();
            match (h264, tiles.is_empty()) {
                (Some(data), true) => {
                    q.avc = g.send_avc420_frame(surface, &data, &regions, ts);
                }
                (Some(data), false) if mixed_ok => {
                    let mut all = Vec::with_capacity(n_tiles + 1);
                    // The AVC tile goes first: a client copies each tile's pixels to its surface
                    // as it processes the PDU (MS-RDPEGFX 3.3.5.1), so the lossless tile sent
                    // after it is what remains where the two overlap.
                    all.push(MixedTilePayload::Avc420 {
                        regions,
                        h264_data: data,
                    });
                    all.extend(tiles);
                    q.avc = g.send_mixed_frame(surface, all, ts);
                    if q.avc.is_some() {
                        q.tiles_with_avc = n_tiles;
                    }
                }
                (Some(data), false) => {
                    // Below capability version 10.4 a client need not process AVC beside another
                    // codec within one frame (MS-RDPEGFX 2.2.3.7), so the two go out as two
                    // frames, the lossless one after the picture it refines.
                    q.avc = g.send_avc420_frame(surface, &data, &regions, ts);
                    if q.avc.is_some() {
                        q.tiles = g.send_mixed_frame(surface, tiles, ts);
                    }
                }
                (None, false) => {
                    // Tiles alone travel in every EGFX version: one codec family is not a mix.
                    q.tiles = g.send_mixed_frame(surface, tiles, ts);
                }
                (None, true) => {}
            }
            (q, g.drain_output(), g.channel_id())
        });
        let sent = match sent {
            Some((q, messages, channel_id)) if q.any() => self.flush(messages, channel_id).then_some(q),
            _ => None,
        };
        let handoff_us = u64::try_from(handoff_started.elapsed().as_micros()).unwrap_or(u64::MAX);

        let he = &mut self.heads[i];
        let Some(queued) = sent else {
            // The encoder state advanced without the client seeing the frame, so the next H.264
            // frame has to be an IDR. Tiles need none — they carry their own pixels — but the
            // ones that did not make it have to come back rather than being left stale.
            he.need_idr |= h264_len > 0;
            if mixed {
                he.blocks.fail_sent(&tile_rects);
            }
            return false;
        };
        he.need_idr = false;
        let sent_tiles = if queued.tiles_with_avc > 0 {
            queued.tiles_with_avc
        } else if queued.tiles.is_some() {
            tile_rects.len()
        } else {
            0
        };
        // Only streams that were queued consume sequence numbers. A frame that never went out,
        // or whose tile half was refused, leaves the counter where it was, so the next tile the
        // client sees still follows on from the last one it saw.
        he.clear.commit(sent_tiles);
        if mixed {
            // The tiles that arrived are the client's exact pixels; the H.264 frame beside them
            // is an approximation wherever no tile covered it.
            he.blocks.mark_sent(&regions_src, &tile_rects[..sent_tiles]);
            // Tiles the client never got — a second frame below 10.4 that was refused — still
            // owe their exact pixels.
            he.blocks.fail_sent(&tile_rects[sent_tiles..]);
        } else if refine {
            // Everything the refinement frame covered has been re-sent sharply.
            he.lossy.clear();
            he.lossy_since = None;
        } else {
            he.lossy.record(&regions_src, frame_qp);
            if he.lossy.is_empty() {
                he.lossy_since = None;
            } else if frame_qp > LOSSY_QP {
                // The area is lossy again: give it time to go still before spending
                // a refinement frame on it.
                he.lossy_since = Some(Instant::now());
            }
        }
        let bytes = h264_len + tile_bytes[..sent_tiles].iter().sum::<usize>();
        self.rate.frames += 1;
        self.rate.bytes += bytes as u64;
        self.shared.frame_sent();
        if let Some(debug) = &self.shared.debug {
            debug.frame_sent(FrameRow {
                head: he.index,
                frame_id: queued.avc.or(queued.tiles).unwrap_or_default(),
                kind: kind_name,
                bytes,
                qp: frame_qp,
                regions: region_count,
                region_area,
                age_us,
                convert_us,
                encode_us,
                handoff_us,
                in_flight: self.shared.frames_in_flight(),
                still_tiles: sent_tiles,
                still_bytes: tile_bytes[..sent_tiles].iter().sum(),
            });
            if let Some(stream) = &video_stream {
                debug.video(he.index, stream);
            }
        }
        true
    }

    /// Once per second: statistics and bitrate adaptation.
    fn tick(&mut self) {
        let elapsed = self.rate.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let fps = (f64::from(self.rate.frames) / secs).round();
        let kbps = (self.rate.bytes as f64 * 8.0 / 1000.0 / secs).round();
        let rtt = self
            .with_gfx(|g| g.qoe_snapshot().map(|q| f64::from(q.avg_rtt_ms)))
            .flatten()
            .filter(|r| r.is_finite() && *r >= 0.0);
        self.shared.core.client_modify(self.shared.sid, |info| {
            info.fps = fps.clamp(0.0, f64::from(u32::MAX)) as u32;
            info.bitrate_kbps = kbps.clamp(0.0, f64::from(u32::MAX)) as u32;
            info.rtt_ms = rtt.map(|r| r.round().clamp(0.0, f64::from(u32::MAX)) as u32);
        });

        if self.rate.auto {
            if self.rate.backpressured {
                self.rate.note_backpressure();
            } else {
                // The queue drained: the congestion that caused it is over.
                self.rate.backpressure_since = None;
            }
            // Congestion that lasts: the link or the decoder cannot keep up, so fewer bits
            // per second are spent. A queue that is full for a moment is not that — dropping
            // the bitrate then would blur the picture to answer a problem that is one frame
            // deep.
            let congested = self
                .rate
                .backpressure_since
                .is_some_and(|since| since.elapsed() >= BACKPRESSURE_GRACE);
            let old = self.rate.cap_kbps;
            let new = if congested {
                (old / 4 * 3).max(AUTO_MIN_KBPS)
            } else if self.rate.backpressured {
                old
            } else {
                (old + old / 10).min(AUTO_MAX_KBPS)
            };
            if new != old {
                self.rate.cap_kbps = new;
                let rate = self.rate.rate();
                for he in &mut self.heads {
                    if he.worker.submit(EncodeJob::Rate(rate)).is_err() {
                        vds_core::warn_ratelimited!(head = he.index, "encoder thread gone, bitrate not changed");
                    }
                }
                debug!(bitrate_kbps = new, "bitrate cap changed");
            }
        }
        self.rate.window_start = Instant::now();
        self.rate.frames = 0;
        self.rate.bytes = 0;
        self.rate.backpressured = false;
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_egfx::pdu::{CapabilitiesV81Flags, CapabilitiesV107Flags, CapabilitySet};

    use super::{LOSSY_MAX_RECTS, LossyArea, avc420_enabled, server_capabilities};
    use vds_core::geom::{Rect, Region};

    fn one(r: Rect) -> Region {
        Region::from_rect(r)
    }

    /// A frame that was sent sharply owes nothing; one that was sent coarsely is remembered
    /// until something covers it at a good quantizer.
    #[test]
    fn lossy_area_tracks_what_was_sent_coarsely() {
        let area = Rect::new(0, 0, 64, 64);
        let mut lossy = LossyArea::default();
        lossy.record(&one(area), 20);
        assert!(lossy.is_empty(), "a sharp frame owes no refinement");

        lossy.record(&one(area), 30);
        assert!(!lossy.is_empty());
        assert_eq!(lossy.region().rects().len(), 1);

        // A sharp frame that covers the area (not necessarily with the same rectangle:
        // damage is merged and split as it arrives) clears it.
        lossy.record(&one(Rect::new(0, 0, 128, 64)), 18);
        assert!(lossy.is_empty());

        // A sharp frame elsewhere leaves it alone.
        lossy.record(&one(area), 30);
        lossy.record(&one(Rect::new(200, 200, 64, 64)), 18);
        assert!(!lossy.is_empty());
    }

    /// The set is bounded: a head that is damaged everywhere must not grow without limit.
    #[test]
    fn lossy_area_is_bounded() {
        let mut lossy = LossyArea::default();
        let extra = 8;
        for i in 0..LOSSY_MAX_RECTS + extra {
            let r = Rect::new(u32::try_from(i).unwrap() * 64, 0, 64, 64);
            lossy.record(&one(r), 30);
        }
        assert_eq!(lossy.rects.len(), LOSSY_MAX_RECTS);
        // The oldest entries go first.
        assert_eq!(lossy.rects[0].0.x, u32::try_from(extra).unwrap() * 64);
    }

    #[test]
    fn avc_flags() {
        assert!(avc420_enabled(&CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE
        }));
        assert!(!avc420_enabled(&CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::AVC_DISABLED
        }));
        assert!(!avc420_enabled(&CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::SMALL_CACHE
        }));
        assert!(avc420_enabled(&CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED
        }));
    }

    #[test]
    fn server_offers_every_flag() {
        let caps = server_capabilities();
        assert!(
            matches!(caps.first(), Some(CapabilitySet::V10_7 { flags }) if flags.contains(CapabilitiesV107Flags::AVC_DISABLED))
        );
    }
}
