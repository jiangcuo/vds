//! Display heads: framebuffers with accumulated damage.
//!
//! A head's pixels come from one of two places:
//!
//! - the shadow buffer: the host copies damaged rectangles into it ([`Head::submit`]);
//! - a host buffer read in place ([`Head::attach_shared`]): the host only reports damage
//!   ([`Head::damage`]) and never copies, and the protocol side reads the host's memory when it
//!   encodes. This takes the copy off the host's main thread.
//!
//! The protocol side takes the accumulated damage and reads the pixels ([`Head::take_frame`]).
//! Pixels are always 32-bit BGRX (QEMU `PIXMAN_x8r8g8b8`, little endian).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::error::{Error, Result};
use crate::geom::{Rect, Region};
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
    /// BGRX pixels, `stride * height` bytes.
    pub pixels: &'a [u8],
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

#[derive(Debug)]
struct HeadState {
    width: u32,
    height: u32,
    /// Shadow buffer, used while no host buffer is attached.
    pixels: Vec<u8>,
    /// Host buffer read in place instead of `pixels`.
    external: Option<External>,
    damage: Region,
    /// When the oldest damage in `damage` arrived; `None` while the buffer is clean.
    damage_at: Option<Instant>,
}

impl HeadState {
    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    /// Starts the damage clock, unless a batch is already open.
    ///
    /// The age a frame reports is measured from the *oldest* damage it carries: when the host
    /// submits twice before a frame is taken, the picture has been stale since the first one.
    fn mark_damage(&mut self, now: Instant) {
        if self.damage_at.is_none() {
            self.damage_at = Some(now);
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// Raised when any head gets damage or the layout changes.
    signal: Signal,
    /// Incremented when heads are added, removed or resized.
    layout_generation: AtomicU64,
    /// A protocol session consumes frames.
    consumer: AtomicBool,
    /// The consumer cannot take a new frame right now (backpressure).
    busy: AtomicBool,
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
            released = s.external.take();
            let bounds = s.bounds();
            s.damage = Region::from_rect(bounds);
            s.mark_damage(Instant::now());
        }
        // Dropped outside the lock: releasing a host buffer calls back into the host.
        drop(released);
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
            released = s.external.replace(External { pixels, stride });
        }
        drop(released);
        Ok(())
    }

    /// Records damage in the attached host buffer (or the shadow buffer) without copying.
    pub fn damage(&self, rects: &[Rect]) {
        {
            let mut s = self.state.lock();
            let bounds = s.bounds();
            for rect in rects {
                let r = rect.intersect(&bounds);
                if !r.is_empty() {
                    s.damage.add(r);
                }
            }
            if s.damage.is_empty() {
                return;
            }
            s.mark_damage(Instant::now());
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
            released = s.external.take();
            let dst_stride = s.stride();
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
                s.damage.add(r);
            }
            if !s.damage.is_empty() {
                s.mark_damage(arrived);
            }
            // What the host thread spent waiting for the head lock and copying.
            tracing::trace!(
                head = self.index,
                wait_us = u64::try_from((locked - arrived).as_micros()).unwrap_or(u64::MAX),
                copy_us = u64::try_from(locked.elapsed().as_micros()).unwrap_or(u64::MAX),
                "submit"
            );
        }
        drop(released);
        self.shared.signal.raise();
        Ok(())
    }

    /// Marks the whole head as damaged (e.g. a new client needs a full frame).
    pub fn invalidate(&self) {
        {
            let mut s = self.state.lock();
            let bounds = s.bounds();
            s.damage = Region::from_rect(bounds);
            s.mark_damage(Instant::now());
        }
        self.shared.signal.raise();
    }

    /// Returns `true` if damage is pending.
    pub fn has_damage(&self) -> bool {
        !self.state.lock().damage.is_empty()
    }

    /// Takes the pending damage and gives read access to the pixels. `f` must not block.
    ///
    /// From the shadow buffer, `f` runs with the head locked and the host waits on that lock
    /// when submitting. From an attached host buffer it runs unlocked: the host never waits.
    ///
    /// The third argument is how long the oldest damage in the batch had been waiting when it
    /// was taken, or `None` if the batch was empty. It is the age of the picture being sent:
    /// the host submitted it that long ago, and the frame has not reached the client yet.
    pub fn take_frame<R>(&self, f: impl FnOnce(FrameRef<'_>, Region, Option<Duration>) -> R) -> R {
        let mut s = self.state.lock();
        let damage = s.damage.take();
        let age = s.damage_at.take().map(|at| at.elapsed());
        if let Some(ext) = s.external.clone() {
            let (width, height) = (s.width, s.height);
            drop(s);
            let frame = FrameRef {
                width,
                height,
                stride: ext.stride,
                pixels: ext.pixels.bytes(),
            };
            return f(frame, damage, age);
        }
        let frame = FrameRef {
            width: s.width,
            height: s.height,
            stride: s.stride(),
            pixels: &s.pixels,
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
            });
        }
        let frame = FrameRef {
            width: s.width,
            height: s.height,
            stride: s.stride(),
            pixels: &s.pixels,
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
                damage: Region::new(),
                damage_at: None,
            }),
            shared: Arc::clone(&self.shared),
        });
        head.resize(width, height)?;
        self.heads.write().insert(index, Arc::clone(&head));
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

    /// Damages every head.
    pub fn invalidate_all(&self) {
        for h in self.heads() {
            h.invalidate();
        }
    }

    /// Declares whether a protocol session consumes frames.
    pub fn set_consumer(&self, active: bool) {
        self.shared.consumer.store(active, Ordering::Release);
        if !active {
            self.shared.busy.store(false, Ordering::Release);
        }
    }

    /// Declares whether the consumer is backpressured.
    pub fn set_busy(&self, busy: bool) {
        self.shared.busy.store(busy, Ordering::Release);
    }

    /// Whether the host should submit frames now (a consumer exists and is not backpressured).
    /// Submitting anyway is allowed; it only costs a copy.
    pub fn wants_frame(&self) -> bool {
        self.shared.consumer.load(Ordering::Acquire) && !self.shared.busy.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BYTES_PER_PIXEL, Display};
    use crate::geom::Rect;

    #[test]
    fn submit_copies_only_damage() {
        let d = Display::new();
        let h = d.add_head(0, 4, 2).unwrap();
        h.take_frame(|_, _, _| ());
        let stride = 4 * BYTES_PER_PIXEL;
        let src = vec![0xAB; stride * 2];
        h.submit(&src, stride, &[Rect::new(1, 1, 2, 1)]).unwrap();
        h.take_frame(|f, damage, _| {
            assert_eq!(damage.rects(), &[Rect::new(1, 1, 2, 1)]);
            assert_eq!(f.pixels[0], 0);
            let off = stride + BYTES_PER_PIXEL;
            assert!(f.pixels[off..off + 2 * BYTES_PER_PIXEL].iter().all(|b| *b == 0xAB));
            assert_eq!(f.pixels[stride + 3 * BYTES_PER_PIXEL], 0);
        });
        assert!(!h.has_damage());
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
        h.take_frame(|_, _, _| ());
        h.submit(&[1; 16], 8, &[Rect::new(1, 1, 100, 100)]).unwrap();
        h.take_frame(|_, damage, _| assert_eq!(damage.rects(), &[Rect::new(1, 1, 1, 1)]));
    }

    /// The age a frame is taken with is measured from the oldest damage it carries, and
    /// taking the frame starts a new batch.
    #[test]
    fn damage_age_is_measured_from_the_oldest_pending_damage() {
        let d = Display::new();
        let h = d.add_head(0, 4, 4).unwrap();
        h.take_frame(|_, _, _| ());
        h.submit(&[0; 64], 16, &[Rect::new(0, 0, 1, 1)]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // The second batch of damage joins the first: the picture has been stale since then.
        h.submit(&[0; 64], 16, &[Rect::new(2, 2, 1, 1)]).unwrap();
        h.take_frame(|_, damage, age| {
            assert_eq!(damage.rects().len(), 2);
            let age = age.expect("the batch carried damage");
            assert!(
                age >= Duration::from_millis(20),
                "age is measured from the first submit: {age:?}"
            );
        });
        // The batch is gone: the next submission starts the clock over.
        h.submit(&[0; 64], 16, &[Rect::new(3, 3, 1, 1)]).unwrap();
        h.take_frame(|_, _, age| {
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
    fn wants_frame_requires_idle_consumer() {
        let d = Display::new();
        assert!(!d.wants_frame());
        d.set_consumer(true);
        assert!(d.wants_frame());
        d.set_busy(true);
        assert!(!d.wants_frame());
        d.set_consumer(false);
        assert!(!d.wants_frame());
    }
}
