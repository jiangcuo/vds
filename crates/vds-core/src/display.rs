//! Display heads: framebuffers with accumulated damage.
//!
//! A head's pixels come from one of three places:
//!
//! - the shadow buffer: the host copies damaged rectangles into it ([`Head::submit`]);
//! - a host buffer read in place ([`Head::attach_shared`]): the host only reports damage
//!   ([`Head::damage`]) and never copies, and the protocol side reads the host's memory when it
//!   encodes. This takes the copy off the host's main thread;
//! - a GPU buffer (a dmabuf, [`Head::attach_gpu`]): the host reports damage and the pixels
//!   never reach the CPU; only a consumer that can import the buffer (the VA-API encoder) reads
//!   it. Consumers that need CPU pixels ask the host for them ([`crate::Core::demand_cpu`]).
//!
//! Every consumer (a client session, [`Display::add_consumer`]) has its own accumulated damage:
//! clients take frames at their own pace, and a client that joins gets a full frame without
//! the others resending theirs. The protocol side takes its damage and reads the pixels
//! ([`Head::take_frame`]).
//! CPU pixels are always 32-bit BGRX (QEMU `PIXMAN_x8r8g8b8`, little endian).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::error::{Error, Result};
use crate::geom::{Rect, Region};
use crate::platform::BufferFd;
use crate::signal::Signal;

/// Bytes per pixel of the shadow framebuffers.
pub const BYTES_PER_PIXEL: usize = 4;

/// Largest accepted head dimension (pixels).
pub const MAX_DIMENSION: u32 = 8192;

/// Read access to a head's pixels.
#[derive(Debug)]
pub struct FrameRef<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    /// BGRX pixels, `stride * height` bytes; **empty** while a GPU buffer is attached: check
    /// [`Self::gpu`] before reading them.
    pub pixels: &'a [u8],
    /// The GPU buffer the head shows, when one is attached ([`Head::attach_gpu`]).
    pub gpu: Option<GpuFrame>,
}

/// A GPU buffer (dmabuf) the host scans out, as given to [`Head::attach_gpu`].
#[derive(Debug)]
pub struct GpuBuffer {
    /// The dmabuf, owned by libvds.
    pub fd: BufferFd,
    /// Width in pixels (the head's width).
    pub width: u32,
    /// Height in pixels (the head's height).
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// `DRM_FORMAT_*` fourcc.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`; `DRM_FORMAT_MOD_INVALID` means the driver's implicit layout.
    pub modifier: u64,
    /// The rows are stored bottom-up (QEMU's `y0_top`): the picture must be flipped.
    pub y0_top: bool,
}

/// Told when the consumer has finished reading a GPU buffer for the damage reported so far.
pub trait ConsumedNotify: Send + Sync + fmt::Debug {
    /// Called once per taken frame, from any thread.
    fn consumed(&self);
}

/// Fires [`ConsumedNotify::consumed`] when the last clone of a frame is dropped.
#[derive(Debug)]
struct ConsumedTicket(Arc<dyn ConsumedNotify>);

impl Drop for ConsumedTicket {
    fn drop(&mut self) {
        self.0.consumed();
    }
}

/// A GPU buffer handed to a consumer with a frame.
///
/// Dropping the last clone tells the host the buffer has been read for that frame, so keep it
/// until the read (the VA-API colour conversion) has been submitted.
#[derive(Debug, Clone)]
pub struct GpuFrame {
    /// The buffer.
    pub buffer: Arc<GpuBuffer>,
    /// Changes whenever another buffer is attached: an importer caches per generation.
    pub generation: u64,
    // Held for its `Drop`: the last clone tells the host.
    _ticket: Option<Arc<ConsumedTicket>>,
}

/// A host framebuffer that libvds reads in place ([`Head::attach_shared`]).
///
/// The host may keep drawing into it while libvds reads: a frame can then mix two pictures
/// (tearing), which the damage of the next frame repairs.
pub trait SharedPixels: Send + Sync + fmt::Debug {
    /// The whole buffer: BGRX rows, `stride` bytes apart as given to `attach_shared`.
    fn bytes(&self) -> &[u8];
}

/// The host buffer a head currently reads, with its row pitch.
#[derive(Debug, Clone)]
struct External {
    pixels: Arc<dyn SharedPixels>,
    stride: usize,
}

/// The GPU buffer a head currently shows.
#[derive(Debug)]
struct Gpu {
    buffer: Arc<GpuBuffer>,
    generation: u64,
    notify: Option<Arc<dyn ConsumedNotify>>,
    /// Damage was reported since the last frame was taken: the host waits for `consumed`.
    pending: bool,
}

impl Gpu {
    /// Lets the buffer go; a host still waiting for this buffer is told it may go on.
    /// Call without the head lock: it calls back into the host.
    fn release(self) {
        if self.pending
            && let Some(n) = self.notify
        {
            n.consumed();
        }
    }
}

#[derive(Debug)]
struct HeadState {
    width: u32,
    height: u32,
    /// Shadow buffer, used while no host buffer is attached.
    pixels: Vec<u8>,
    /// Host buffer read in place instead of `pixels`.
    external: Option<External>,
    /// GPU buffer shown instead of `pixels` (never together with `external`).
    gpu: Option<Gpu>,
    /// Generation of the last GPU buffer attached.
    gpu_generation: u64,
    /// Damage not yet taken, per consumer.
    damage: BTreeMap<ConsumerId, Pending>,
}

/// A consumer's damage not yet taken.
#[derive(Debug, Default)]
struct Pending {
    region: Region,
    /// When the oldest damage in `region` arrived; `None` while it is clean.
    since: Option<Instant>,
}

impl Pending {
    /// Starts the damage clock, unless a batch is already open.
    ///
    /// The age a frame reports is measured from the *oldest* damage it carries: when the host
    /// submits twice before a frame is taken, the picture has been stale since the first one.
    fn mark(&mut self, now: Instant) {
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    fn full(bounds: Rect, now: Instant) -> Self {
        Self {
            region: Region::from_rect(bounds),
            since: Some(now),
        }
    }
}

impl HeadState {
    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    /// Every consumer needs the whole head.
    fn damage_full(&mut self, now: Instant) {
        let bounds = self.bounds();
        for p in self.damage.values_mut() {
            *p = Pending::full(bounds, now);
        }
    }

    /// Adds `rects` (already clipped, non-empty) to every consumer's damage.
    fn damage_rects(&mut self, rects: &[Rect], now: Instant) {
        if rects.is_empty() {
            return;
        }
        for p in self.damage.values_mut() {
            for r in rects {
                p.region.add(*r);
            }
            p.mark(now);
        }
    }
}

/// A consumer of frames: a client session, identified by its session id.
pub type ConsumerId = u64;

#[derive(Debug, Default)]
struct Shared {
    /// Raised when any head gets damage or the layout changes.
    signal: Signal,
    /// Incremented when heads are added, removed or resized.
    layout_generation: AtomicU64,
    /// Consumers, and whether each cannot take a new frame right now (backpressure). Also
    /// held while heads are added, so a head and a consumer never miss each other.
    consumers: Mutex<BTreeMap<ConsumerId, bool>>,
}

/// One display output (QEMU console).
#[derive(Debug)]
pub struct Head {
    index: u32,
    state: Mutex<HeadState>,
    shared: Arc<Shared>,
}

impl Head {
    /// Head index (QEMU console index).
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Current size in pixels.
    pub fn size(&self) -> (u32, u32) {
        let s = self.state.lock();
        (s.width, s.height)
    }

    /// Changes the size; the content is cleared and fully damaged. An attached host buffer
    /// is let go: it has the old geometry.
    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::InvalidArgument(format!("head size {width}x{height}")));
        }
        let released;
        {
            let mut s = self.state.lock();
            if s.width == width && s.height == height {
                return Ok(());
            }
            s.width = width;
            s.height = height;
            s.pixels.clear();
            s.pixels.resize(width as usize * height as usize * BYTES_PER_PIXEL, 0);
            released = (s.external.take(), s.gpu.take());
            s.damage_full(Instant::now());
        }
        // Dropped outside the lock: releasing a host buffer calls back into the host.
        drop(released.0);
        if let Some(g) = released.1 {
            g.release();
        }
        self.shared.layout_generation.fetch_add(1, Ordering::AcqRel);
        self.shared.signal.raise();
        Ok(())
    }

    /// Reads `pixels` in place from now on (BGRX, `stride` bytes per row, head geometry),
    /// instead of the shadow buffer. The previous host buffer, if any, is let go.
    ///
    /// Nothing is marked damaged: the host reports what changed with [`Head::damage`].
    pub fn attach_shared(&self, pixels: Arc<dyn SharedPixels>, stride: usize) -> Result<()> {
        let released;
        {
            let mut s = self.state.lock();
            let row = s.stride();
            if stride < row {
                return Err(Error::InvalidArgument(format!("stride {stride} below row size {row}")));
            }
            let needed = stride * (s.height as usize - 1) + row;
            let len = pixels.bytes().len();
            if len < needed {
                return Err(Error::InvalidArgument(format!(
                    "buffer of {len} bytes too small, need {needed}"
                )));
            }
            released = (s.external.replace(External { pixels, stride }), s.gpu.take());
        }
        drop(released.0);
        if let Some(g) = released.1 {
            g.release();
        }
        Ok(())
    }

    /// Shows the GPU buffer `buffer` from now on (same geometry as the head). The previous
    /// host or GPU buffer, if any, is let go. The whole head is marked damaged: it shows a new
    /// picture. `notify`, if given, is told whenever a taken frame has been read.
    pub fn attach_gpu(&self, buffer: GpuBuffer, notify: Option<Arc<dyn ConsumedNotify>>) -> Result<()> {
        let released;
        {
            let mut s = self.state.lock();
            if buffer.width != s.width || buffer.height != s.height {
                return Err(Error::InvalidArgument(format!(
                    "buffer is {}x{}, head is {}x{}",
                    buffer.width, buffer.height, s.width, s.height
                )));
            }
            if buffer.stride < buffer.width.saturating_mul(4) {
                return Err(Error::InvalidArgument(format!(
                    "stride {} below row size {}",
                    buffer.stride,
                    buffer.width.saturating_mul(4)
                )));
            }
            s.gpu_generation += 1;
            let generation = s.gpu_generation;
            released = (
                s.external.take(),
                s.gpu.replace(Gpu {
                    buffer: Arc::new(buffer),
                    generation,
                    notify,
                    pending: false,
                }),
            );
            s.damage_full(Instant::now());
        }
        drop(released.0);
        if let Some(g) = released.1 {
            g.release();
        }
        self.shared.signal.raise();
        Ok(())
    }

    /// Lets the host or GPU buffer go and reads the shadow buffer again, cleared to black and
    /// fully damaged.
    pub fn detach(&self) {
        let released;
        {
            let mut s = self.state.lock();
            released = (s.external.take(), s.gpu.take());
            s.pixels.fill(0);
            s.damage_full(Instant::now());
        }
        drop(released.0);
        if let Some(g) = released.1 {
            g.release();
        }
        self.shared.signal.raise();
    }

    /// Whether a GPU buffer is attached.
    pub fn has_gpu(&self) -> bool {
        self.state.lock().gpu.is_some()
    }

    /// Records damage in the attached host buffer (or the shadow buffer) without copying.
    pub fn damage(&self, rects: &[Rect]) {
        {
            let mut s = self.state.lock();
            let bounds = s.bounds();
            let clipped: Vec<Rect> = rects
                .iter()
                .map(|r| r.intersect(&bounds))
                .filter(|r| !r.is_empty())
                .collect();
            if clipped.is_empty() {
                return;
            }
            s.damage_rects(&clipped, Instant::now());
            if let Some(g) = s.gpu.as_mut() {
                g.pending = true;
            }
        }
        self.shared.signal.raise();
    }

    /// Copies the damaged `rects` of `src` (BGRX, `stride` bytes per row, same geometry as
    /// the head) into the shadow buffer and records the damage. An attached host buffer is
    /// let go: the shadow buffer is what gets read from now on.
    pub fn submit(&self, src: &[u8], stride: usize, rects: &[Rect]) -> Result<()> {
        let arrived = Instant::now();
        let released;
        {
            let mut s = self.state.lock();
            let locked = Instant::now();
            let bounds = s.bounds();
            if stride < s.stride() {
                return Err(Error::InvalidArgument(format!(
                    "stride {stride} below row size {}",
                    s.stride()
                )));
            }
            let needed = stride * (s.height as usize - 1) + s.stride();
            if src.len() < needed {
                return Err(Error::InvalidArgument(format!(
                    "buffer of {} bytes too small, need {needed}",
                    src.len()
                )));
            }
            released = (s.external.take(), s.gpu.take());
            let dst_stride = s.stride();
            let mut copied = Vec::with_capacity(rects.len());
            for rect in rects {
                let r = rect.intersect(&bounds);
                if r.is_empty() {
                    continue;
                }
                let row_bytes = r.w as usize * BYTES_PER_PIXEL;
                let x_off = r.x as usize * BYTES_PER_PIXEL;
                for y in r.y..r.bottom() {
                    let so = y as usize * stride + x_off;
                    let doff = y as usize * dst_stride + x_off;
                    s.pixels[doff..doff + row_bytes].copy_from_slice(&src[so..so + row_bytes]);
                }
                copied.push(r);
            }
            s.damage_rects(&copied, arrived);
            // What the host thread spent waiting for the head lock and copying.
            tracing::trace!(
                head = self.index,
                wait_us = u64::try_from((locked - arrived).as_micros()).unwrap_or(u64::MAX),
                copy_us = u64::try_from(locked.elapsed().as_micros()).unwrap_or(u64::MAX),
                "submit"
            );
        }
        drop(released.0);
        if let Some(g) = released.1 {
            g.release();
        }
        self.shared.signal.raise();
        Ok(())
    }

    /// Marks the whole head as damaged for `consumer` (e.g. it needs a full frame); the other
    /// consumers are not affected.
    pub fn invalidate(&self, consumer: ConsumerId) {
        {
            let mut s = self.state.lock();
            let bounds = s.bounds();
            if let Some(p) = s.damage.get_mut(&consumer) {
                *p = Pending::full(bounds, Instant::now());
            }
        }
        self.shared.signal.raise();
    }

    /// Returns `true` if damage is pending for `consumer`.
    pub fn has_damage(&self, consumer: ConsumerId) -> bool {
        self.state
            .lock()
            .damage
            .get(&consumer)
            .is_some_and(|p| !p.region.is_empty())
    }

    /// Takes `consumer`'s pending damage and gives read access to the pixels. `f` must not
    /// block. An unknown consumer gets no damage.
    ///
    /// From the shadow buffer, `f` runs with the head locked and the host waits on that lock
    /// when submitting. From an attached host buffer it runs unlocked: the host never waits.
    ///
    /// The third argument is how long the oldest damage in the batch had been waiting when it
    /// was taken, or `None` if the batch was empty. It is the age of the picture being sent:
    /// the host submitted it that long ago, and the frame has not reached the client yet.
    ///
    /// With a GPU buffer attached, the frame carries it ([`FrameRef::gpu`]); the host is told
    /// the buffer has been read once every clone of that [`GpuFrame`] is dropped.
    pub fn take_frame<R>(
        &self,
        consumer: ConsumerId,
        f: impl FnOnce(FrameRef<'_>, Region, Option<Duration>) -> R,
    ) -> R {
        let mut s = self.state.lock();
        let (damage, age) = match s.damage.get_mut(&consumer) {
            Some(p) => (p.region.take(), p.since.take().map(|at| at.elapsed())),
            None => (Region::new(), None),
        };
        if let Some(ext) = s.external.clone() {
            let (width, height) = (s.width, s.height);
            drop(s);
            let frame = FrameRef {
                width,
                height,
                stride: ext.stride,
                pixels: ext.pixels.bytes(),
                gpu: None,
            };
            return f(frame, damage, age);
        }
        let gpu = s.gpu.as_mut().map(|g| {
            let ticket = if core::mem::take(&mut g.pending) {
                g.notify.clone().map(|n| Arc::new(ConsumedTicket(n)))
            } else {
                None
            };
            GpuFrame {
                buffer: Arc::clone(&g.buffer),
                generation: g.generation,
                _ticket: ticket,
            }
        });
        let (width, height, stride) = (s.width, s.height, s.stride());
        if gpu.is_some() {
            // The shadow buffer is not what the head shows, so it is not lent out; unlocked,
            // because dropping the ticket calls the host.
            drop(s);
            return f(
                FrameRef {
                    width,
                    height,
                    stride,
                    pixels: &[],
                    gpu,
                },
                damage,
                age,
            );
        }
        let frame = FrameRef {
            width,
            height,
            stride,
            pixels: &s.pixels,
            gpu: None,
        };
        f(frame, damage, age)
    }

    /// Gives read access to the pixels without touching the damage.
    pub fn with_frame<R>(&self, f: impl FnOnce(FrameRef<'_>) -> R) -> R {
        let s = self.state.lock();
        if let Some(ext) = s.external.clone() {
            let (width, height) = (s.width, s.height);
            drop(s);
            return f(FrameRef {
                width,
                height,
                stride: ext.stride,
                pixels: ext.pixels.bytes(),
                gpu: None,
            });
        }
        if let Some(g) = s.gpu.as_ref() {
            let gpu = GpuFrame {
                buffer: Arc::clone(&g.buffer),
                generation: g.generation,
                _ticket: None,
            };
            let (width, height, stride) = (s.width, s.height, s.stride());
            drop(s);
            return f(FrameRef {
                width,
                height,
                stride,
                pixels: &[],
                gpu: Some(gpu),
            });
        }
        let frame = FrameRef {
            width: s.width,
            height: s.height,
            stride: s.stride(),
            pixels: &s.pixels,
            gpu: None,
        };
        f(frame)
    }
}

/// Position of a head on the virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadLayout {
    /// Head index.
    pub index: u32,
    /// Left edge on the desktop.
    pub x: u32,
    /// Top edge on the desktop (always 0: heads are placed side by side).
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// All heads of the VM.
#[derive(Debug, Default)]
pub struct Display {
    heads: RwLock<BTreeMap<u32, Arc<Head>>>,
    shared: Arc<Shared>,
}

impl Display {
    /// Creates an empty display.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a head (or returns the existing one) with an initial size.
    pub fn add_head(&self, index: u32, width: u32, height: u32) -> Result<Arc<Head>> {
        // Held until the head is in the map: a consumer added meanwhile would miss it.
        let consumers = self.shared.consumers.lock();
        if let Some(h) = self.heads.read().get(&index) {
            return Ok(Arc::clone(h));
        }
        let head = Arc::new(Head {
            index,
            state: Mutex::new(HeadState {
                width: 0,
                height: 0,
                pixels: Vec::new(),
                external: None,
                gpu: None,
                gpu_generation: 0,
                damage: consumers.keys().map(|c| (*c, Pending::default())).collect(),
            }),
            shared: Arc::clone(&self.shared),
        });
        head.resize(width, height)?;
        self.heads.write().insert(index, Arc::clone(&head));
        drop(consumers);
        self.shared.layout_generation.fetch_add(1, Ordering::AcqRel);
        self.shared.signal.raise();
        Ok(head)
    }

    /// Removes a head.
    pub fn remove_head(&self, index: u32) {
        if self.heads.write().remove(&index).is_some() {
            self.shared.layout_generation.fetch_add(1, Ordering::AcqRel);
            self.shared.signal.raise();
        }
    }

    /// Looks up a head.
    pub fn head(&self, index: u32) -> Option<Arc<Head>> {
        self.heads.read().get(&index).cloned()
    }

    /// All heads ordered by index.
    pub fn heads(&self) -> Vec<Arc<Head>> {
        self.heads.read().values().cloned().collect()
    }

    /// Heads placed side by side, ordered by index.
    pub fn layout(&self) -> Vec<HeadLayout> {
        let mut x = 0u32;
        self.heads
            .read()
            .values()
            .map(|h| {
                let (width, height) = h.size();
                let l = HeadLayout {
                    index: h.index,
                    x,
                    y: 0,
                    width,
                    height,
                };
                x = x.saturating_add(width);
                l
            })
            .collect()
    }

    /// Size of the virtual desktop containing every head (`(0, 0)` without heads).
    pub fn desktop_size(&self) -> (u32, u32) {
        self.layout()
            .iter()
            .fold((0, 0), |(w, h), l| (w.max(l.x + l.width), h.max(l.y + l.height)))
    }

    /// Generation counter of the layout; changes whenever heads are added, removed or resized.
    pub fn layout_generation(&self) -> u64 {
        self.shared.layout_generation.load(Ordering::Acquire)
    }

    /// Signal raised on damage and layout changes.
    pub fn signal(&self) -> &Signal {
        &self.shared.signal
    }

    /// Damages every head for `consumer`: it needs a full frame.
    pub fn invalidate(&self, consumer: ConsumerId) {
        for h in self.heads() {
            h.invalidate(consumer);
        }
    }

    /// Starts collecting damage for `consumer`; it first needs every head whole.
    pub fn add_consumer(&self, consumer: ConsumerId) {
        let mut consumers = self.shared.consumers.lock();
        consumers.insert(consumer, false);
        let now = Instant::now();
        for h in self.heads.read().values() {
            let mut s = h.state.lock();
            let bounds = s.bounds();
            s.damage.insert(consumer, Pending::full(bounds, now));
        }
        drop(consumers);
        self.shared.signal.raise();
    }

    /// Stops collecting damage for `consumer`.
    pub fn remove_consumer(&self, consumer: ConsumerId) {
        let mut consumers = self.shared.consumers.lock();
        consumers.remove(&consumer);
        for h in self.heads.read().values() {
            h.state.lock().damage.remove(&consumer);
        }
    }

    /// Declares whether `consumer` is backpressured.
    pub fn set_busy(&self, consumer: ConsumerId, busy: bool) {
        if let Some(b) = self.shared.consumers.lock().get_mut(&consumer) {
            *b = busy;
        }
    }

    /// Whether the host should submit frames now: some consumer is not backpressured.
    /// Submitting anyway is allowed; it only costs a copy.
    pub fn wants_frame(&self) -> bool {
        self.shared.consumers.lock().values().any(|busy| !busy)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BYTES_PER_PIXEL, ConsumerId, Display};
    use crate::geom::Rect;

    const C: ConsumerId = 1;

    #[test]
    fn submit_copies_only_damage() {
        let d = Display::new();
        let h = d.add_head(0, 4, 2).unwrap();
        d.add_consumer(C);
        h.take_frame(C, |_, _, _| ());
        let stride = 4 * BYTES_PER_PIXEL;
        let src = vec![0xAB; stride * 2];
        h.submit(&src, stride, &[Rect::new(1, 1, 2, 1)]).unwrap();
        h.take_frame(C, |f, damage, _| {
            assert_eq!(damage.rects(), &[Rect::new(1, 1, 2, 1)]);
            assert_eq!(f.pixels[0], 0);
            let off = stride + BYTES_PER_PIXEL;
            assert!(f.pixels[off..off + 2 * BYTES_PER_PIXEL].iter().all(|b| *b == 0xAB));
            assert_eq!(f.pixels[stride + 3 * BYTES_PER_PIXEL], 0);
        });
        assert!(!h.has_damage(C));
    }

    #[test]
    fn submit_rejects_short_buffer() {
        let d = Display::new();
        let h = d.add_head(0, 4, 4).unwrap();
        assert!(h.submit(&[0; 8], 16, &[Rect::new(0, 0, 1, 1)]).is_err());
    }

    #[test]
    fn rects_are_clipped() {
        let d = Display::new();
        let h = d.add_head(0, 2, 2).unwrap();
        d.add_consumer(C);
        h.take_frame(C, |_, _, _| ());
        h.submit(&[1; 16], 8, &[Rect::new(1, 1, 100, 100)]).unwrap();
        h.take_frame(C, |_, damage, _| assert_eq!(damage.rects(), &[Rect::new(1, 1, 1, 1)]));
    }

    /// The age a frame is taken with is measured from the oldest damage it carries, and
    /// taking the frame starts a new batch.
    #[test]
    fn damage_age_is_measured_from_the_oldest_pending_damage() {
        let d = Display::new();
        let h = d.add_head(0, 4, 4).unwrap();
        d.add_consumer(C);
        h.take_frame(C, |_, _, _| ());
        h.submit(&[0; 64], 16, &[Rect::new(0, 0, 1, 1)]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // The second batch of damage joins the first: the picture has been stale since then.
        h.submit(&[0; 64], 16, &[Rect::new(2, 2, 1, 1)]).unwrap();
        h.take_frame(C, |_, damage, age| {
            assert_eq!(damage.rects().len(), 2);
            let age = age.expect("the batch carried damage");
            assert!(
                age >= Duration::from_millis(20),
                "age is measured from the first submit: {age:?}"
            );
        });
        // The batch is gone: the next submission starts the clock over.
        h.submit(&[0; 64], 16, &[Rect::new(3, 3, 1, 1)]).unwrap();
        h.take_frame(C, |_, _, age| {
            let age = age.expect("the new batch carried damage");
            assert!(age < Duration::from_millis(20), "the clock restarted: {age:?}");
        });
    }

    #[test]
    fn layout_places_heads_side_by_side() {
        let d = Display::new();
        d.add_head(1, 800, 600).unwrap();
        d.add_head(0, 1024, 768).unwrap();
        let l = d.layout();
        assert_eq!(l[0].index, 0);
        assert_eq!(l[1].x, 1024);
        assert_eq!(d.desktop_size(), (1824, 768));
    }

    #[test]
    fn layout_generation_changes_on_resize() {
        let d = Display::new();
        let h = d.add_head(0, 10, 10).unwrap();
        let g = d.layout_generation();
        h.resize(20, 10).unwrap();
        assert!(d.layout_generation() > g);
        let g = d.layout_generation();
        h.resize(20, 10).unwrap();
        assert_eq!(d.layout_generation(), g);
    }

    #[test]
    fn wants_frame_requires_an_idle_consumer() {
        let d = Display::new();
        assert!(!d.wants_frame());
        d.add_consumer(1);
        d.add_consumer(2);
        assert!(d.wants_frame());
        d.set_busy(1, true);
        assert!(d.wants_frame(), "consumer 2 still takes frames");
        d.set_busy(2, true);
        assert!(!d.wants_frame());
        d.remove_consumer(2);
        assert!(!d.wants_frame());
        d.remove_consumer(1);
        assert!(!d.wants_frame());
    }

    /// Each consumer takes its own damage: one taking a frame does not take it from another.
    #[test]
    fn consumers_take_damage_independently() {
        let d = Display::new();
        let h = d.add_head(0, 4, 4).unwrap();
        d.add_consumer(1);
        h.take_frame(1, |_, _, _| ());
        h.submit(&[0; 64], 16, &[Rect::new(0, 0, 1, 1)]).unwrap();
        // A consumer that joins later starts with the whole head, plus nothing older.
        d.add_consumer(2);
        h.take_frame(1, |_, damage, _| assert_eq!(damage.rects(), &[Rect::new(0, 0, 1, 1)]));
        assert!(!h.has_damage(1));
        assert!(h.has_damage(2));
        h.take_frame(2, |_, damage, _| assert_eq!(damage.rects(), &[Rect::new(0, 0, 4, 4)]));
        h.submit(&[0; 64], 16, &[Rect::new(2, 2, 1, 1)]).unwrap();
        h.take_frame(2, |_, damage, _| assert_eq!(damage.rects(), &[Rect::new(2, 2, 1, 1)]));
        h.take_frame(1, |_, damage, _| assert_eq!(damage.rects(), &[Rect::new(2, 2, 1, 1)]));
    }

    /// A new client's full frame is its own: the others are not made to resend everything.
    #[test]
    fn invalidate_touches_one_consumer() {
        let d = Display::new();
        let h = d.add_head(0, 4, 4).unwrap();
        d.add_consumer(1);
        d.add_consumer(2);
        h.take_frame(1, |_, _, _| ());
        h.take_frame(2, |_, _, _| ());
        d.invalidate(2);
        assert!(!h.has_damage(1));
        assert!(h.has_damage(2));
    }

    /// Damage is kept only for registered consumers, and a head added later knows them.
    #[test]
    fn heads_and_consumers_find_each_other() {
        let d = Display::new();
        d.add_consumer(7);
        let h = d.add_head(0, 4, 4).unwrap();
        assert!(h.has_damage(7), "a new head is new to every consumer");
        d.remove_consumer(7);
        h.submit(&[0; 64], 16, &[Rect::new(0, 0, 1, 1)]).unwrap();
        assert!(!h.has_damage(7));
        h.take_frame(7, |_, damage, age| {
            assert!(damage.is_empty());
            assert!(age.is_none());
        });
    }
}
