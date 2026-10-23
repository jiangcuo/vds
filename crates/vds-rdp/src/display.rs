//! `RdpServerDisplay` implementation: codec selection and the legacy
//! update stream (RemoteFX / bitmap, chosen by IronRDP from the client capabilities), plus
//! pointer updates for both transports.

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BitmapUpdate, ColorPointer, DesktopSize, DisplayUpdate, PixelFormat, RGBAPointer, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerResult,
};
use tokio::time::Instant;
use tracing::{debug, info};
use vds_core::codec::{self, Decision, EgfxStatus, Transport};
use vds_core::cursor::CursorState;
use vds_core::debug::PointerRow;
use vds_core::display::{BYTES_PER_PIXEL, FrameRef};
use vds_core::event::{ClientEvent, MonitorLayout};
use vds_core::geom::Rect;

use crate::session::SessionShared;

/// Size advertised before the guest has a display.
const FALLBACK_SIZE: (u32, u32) = (1024, 768);
/// Size of the transparent pointer that stands for "no client cursor".
const TRANSPARENT_POINTER: u16 = 32;

/// Largest pointer sent; larger guest cursors are cropped (not every client supports large
/// pointers).
const MAX_POINTER: u32 = 96;

fn desktop_size(w: u32, h: u32) -> DesktopSize {
    DesktopSize {
        width: u16::try_from(w).unwrap_or(u16::MAX),
        height: u16::try_from(h).unwrap_or(u16::MAX),
    }
}

pub(crate) struct DisplayHandler {
    shared: Arc<SessionShared>,
}

impl DisplayHandler {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self { shared }
    }

    /// Size of the legacy desktop: the first head.
    fn legacy_size(&self) -> (u32, u32) {
        self.shared
            .core
            .display()
            .heads()
            .first()
            .map_or(FALLBACK_SIZE, |h| h.size())
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for DisplayHandler {
    async fn size(&mut self) -> DesktopSize {
        let (w, h) = self.legacy_size();
        *self.shared.client_desktop.lock() = Some((w, h));
        desktop_size(w, h)
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        self.shared.client.set_desktop(client_size.width, client_size.height);
        if self.shared.core.config().resize_on_connect {
            debug!(w = client_size.width, h = client_size.height, "requesting client size");
            self.shared.core.push_event(ClientEvent::Layout(vec![MonitorLayout {
                left: 0,
                top: 0,
                width: u32::from(client_size.width),
                height: u32::from(client_size.height),
                width_mm: 0,
                height_mm: 0,
                primary: true,
            }]));
        }
        self.size().await
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        self.shared.activate();
        Ok(Box::new(DisplayUpdates::new(Arc::clone(&self.shared))))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitors: Vec<MonitorLayout> = layout
            .monitors()
            .iter()
            .map(|m| {
                let (left, top) = m.position().unwrap_or_default();
                let (width, height) = m.dimensions();
                let (width_mm, height_mm) = m.physical_dimensions().unwrap_or_default();
                MonitorLayout {
                    left,
                    top,
                    width,
                    height,
                    width_mm,
                    height_mm,
                    primary: m.is_primary(),
                }
            })
            .collect();
        debug!(monitors = monitors.len(), "client requested a monitor layout");
        if !monitors.is_empty() {
            self.shared.core.push_event(ClientEvent::Layout(monitors));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Undecided,
    Legacy,
    Egfx,
}

struct DisplayUpdates {
    shared: Arc<SessionShared>,
    mode: Mode,
    grace_deadline: Instant,
    /// Generation of the last pointer update sent; `None` until the first one, which is
    /// always sent: without it the client keeps its own default arrow on top of the cursor
    /// the guest draws into the framebuffer (std VGA, bochs, ramfb: no hardware cursor).
    cursor_generation: Option<u64>,
    queue: VecDeque<DisplayUpdate>,
    next_frame_at: Instant,
}

impl DisplayUpdates {
    fn new(shared: Arc<SessionShared>) -> Self {
        let grace = Duration::from_millis(u64::from(shared.core.config().egfx_grace_ms));
        // After a deactivation-reactivation the transport is already known.
        let mode = match shared.transport() {
            None => Mode::Undecided,
            Some(Transport::Legacy) => Mode::Legacy,
            Some(Transport::Egfx) => Mode::Egfx,
        };
        if mode == Mode::Legacy {
            shared.core.display().invalidate_all();
        }
        Self {
            shared,
            mode,
            grace_deadline: Instant::now() + grace,
            cursor_generation: None,
            queue: VecDeque::new(),
            next_frame_at: Instant::now(),
        }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_millis(1000 / u64::from(self.shared.core.config().max_fps.max(1)))
    }

    fn effective_egfx(&self) -> EgfxStatus {
        match self.shared.egfx_status() {
            EgfxStatus::Pending if Instant::now() >= self.grace_deadline => EgfxStatus::Unavailable,
            s => s,
        }
    }

    fn enter_egfx(&mut self) {
        info!(codec = "avc420", "graphics transport selected");
        self.mode = Mode::Egfx;
        self.shared.set_transport(Transport::Egfx);
        self.shared.start_video();
    }

    fn enter_legacy(&mut self) {
        let was_egfx = self.mode == Mode::Egfx;
        info!(codec = "legacy", "graphics transport selected");
        self.mode = Mode::Legacy;
        self.shared.set_transport(Transport::Legacy);
        if was_egfx {
            self.shared.stop_video();
            // The client's legacy desktop is unknown after EGFX: force a resize.
            *self.shared.client_desktop.lock() = None;
        }
        let display = self.shared.core.display();
        display.set_busy(false);
        display.invalidate_all();
    }

    /// A fully transparent pointer.
    ///
    /// Used instead of `DisplayUpdate::HidePointer`: IronRDP's fast-path fragmenter emits
    /// nothing for updates without payload, so the hidden (and default) system pointer
    /// updates never reach the client.
    fn transparent_pointer() -> DisplayUpdate {
        DisplayUpdate::ColorPointer(ColorPointer {
            cache_index: 0,
            width: TRANSPARENT_POINTER,
            height: TRANSPARENT_POINTER,
            hot_x: 0,
            hot_y: 0,
            and_mask: vec![0xff; usize::from(TRANSPARENT_POINTER).div_ceil(16) * 2 * usize::from(TRANSPARENT_POINTER)],
            xor_mask: vec![0; usize::from(TRANSPARENT_POINTER) * usize::from(TRANSPARENT_POINTER) * 3],
        })
    }

    fn cursor_update(state: &CursorState) -> (DisplayUpdate, PointerRow) {
        match (&state.shape, state.visible) {
            // Without a shape the guest draws the cursor into the framebuffer itself
            // (no hardware cursor): the client must not draw a second one.
            (None, _) | (_, false) => (
                Self::transparent_pointer(),
                PointerRow {
                    kind: "transparent",
                    width: TRANSPARENT_POINTER,
                    height: TRANSPARENT_POINTER,
                    hot_x: 0,
                    hot_y: 0,
                    pdu: "color",
                    and_mask_len: usize::from(TRANSPARENT_POINTER).div_ceil(16) * 2 * usize::from(TRANSPARENT_POINTER),
                    dropped: None,
                },
            ),
            (Some(img), true) => {
                let w = img.width.min(MAX_POINTER);
                let h = img.height.min(MAX_POINTER);
                // RDP 32bpp pointers are bottom-up BGRA.
                let mut data = Vec::with_capacity(w as usize * h as usize * 4);
                for y in (0..h).rev() {
                    let row = y as usize * img.width as usize;
                    for x in 0..w as usize {
                        data.extend_from_slice(&img.argb[row + x].to_le_bytes());
                    }
                }
                let update = DisplayUpdate::RGBAPointer(RGBAPointer {
                    cache_index: 0,
                    width: u16::try_from(w).unwrap_or(1),
                    height: u16::try_from(h).unwrap_or(1),
                    hot_x: u16::try_from(img.hot_x.min(w - 1)).unwrap_or(0),
                    hot_y: u16::try_from(img.hot_y.min(h - 1)).unwrap_or(0),
                    data,
                });
                (
                    update,
                    PointerRow {
                        kind: "shape",
                        width: u16::try_from(w).unwrap_or(1),
                        height: u16::try_from(h).unwrap_or(1),
                        hot_x: u16::try_from(img.hot_x.min(w.saturating_sub(1))).unwrap_or(0),
                        hot_y: u16::try_from(img.hot_y.min(h.saturating_sub(1))).unwrap_or(0),
                        pdu: "rgba",
                        // IronRDP's RGBA pointer encoder always writes an empty AND mask,
                        // so this is 0 for shape
                        // updates.
                        and_mask_len: 0,
                        dropped: None,
                    },
                )
            }
        }
    }

    /// Splits `r` into rectangles whose widths are multiples of 4 pixels (IronRDP's bitmap
    /// encoder requires it), widening to the left at the right edge. A rectangle that cannot
    /// be widened (frame width not a multiple of 4) is covered by two overlapping ones.
    fn align_width(r: Rect, frame_width: u32) -> Vec<Rect> {
        let w = r.w.div_ceil(4) * 4;
        if frame_width < 4 {
            return Vec::new();
        }
        if w > frame_width {
            let main = frame_width & !3;
            return vec![Rect::new(0, r.y, main, r.h), Rect::new(frame_width - 4, r.y, 4, r.h)];
        }
        let x = (r.x & !3).min(frame_width - w);
        vec![Rect::new(x, r.y, w, r.h)]
    }

    fn bitmap(frame: &FrameRef<'_>, r: Rect) -> Option<DisplayUpdate> {
        let width = NonZeroU16::new(u16::try_from(r.w).ok()?)?;
        let height = NonZeroU16::new(u16::try_from(r.h).ok()?)?;
        let row = r.w as usize * BYTES_PER_PIXEL;
        let mut data = Vec::with_capacity(row * r.h as usize);
        for y in r.y..r.bottom() {
            let off = y as usize * frame.stride + r.x as usize * BYTES_PER_PIXEL;
            data.extend_from_slice(frame.pixels.get(off..off + row)?);
        }
        Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: u16::try_from(r.x).ok()?,
            y: u16::try_from(r.y).ok()?,
            width,
            height,
            format: PixelFormat::BgrX32,
            data: Bytes::from(data),
            stride: NonZeroUsize::new(row)?,
        }))
    }

    /// Waits for any of the given wake-up sources, or `deadline`.
    async fn wait(&self, display: bool, deadline: Option<Instant>) {
        let core = &self.shared.core;
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => core::future::pending().await,
            }
        };
        let display_wait = async {
            if display {
                core.display().signal().wait().await;
            } else {
                core::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = core.cursor().signal().wait() => {}
            () = self.shared.mode_signal.wait() => {}
            () = display_wait => {}
            () = sleep => {}
        }
    }

    /// One step of the legacy transport. Returns `true` if updates were queued.
    async fn legacy_step(&mut self) -> bool {
        let display = self.shared.core.display();
        let Some(head) = display.heads().into_iter().next() else {
            self.wait(true, None).await;
            return false;
        };
        let size = head.size();
        {
            let mut known = self.shared.client_desktop.lock();
            if *known != Some(size) {
                *known = Some(size);
                debug!(w = size.0, h = size.1, "legacy desktop resize");
                self.queue
                    .push_back(DisplayUpdate::Resize(desktop_size(size.0, size.1)));
                return true;
            }
        }
        if head.has_damage() {
            let now = Instant::now();
            if now < self.next_frame_at {
                let at = self.next_frame_at;
                self.wait(false, Some(at)).await;
                return false;
            }
            self.next_frame_at = now + self.frame_interval();
            let updates: Vec<_> = head.take_frame(|frame, damage, _| {
                damage
                    .rects()
                    .iter()
                    .flat_map(|r| Self::align_width(*r, frame.width))
                    .filter_map(|r| Self::bitmap(&frame, r))
                    .collect()
            });
            let queued = !updates.is_empty();
            debug!(bitmaps = updates.len(), "legacy frame queued");
            self.queue.extend(updates);
            return queued;
        }
        self.wait(true, None).await;
        false
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for DisplayUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        loop {
            if let Some(u) = self.queue.pop_front() {
                return Ok(Some(u));
            }
            if self.shared.is_stopping() {
                return Ok(None);
            }

            let cursor = self.shared.core.cursor().snapshot();
            if self.cursor_generation != Some(cursor.generation) {
                self.cursor_generation = Some(cursor.generation);
                let (update, row) = Self::cursor_update(&cursor);
                if let Some(debug) = &self.shared.debug {
                    debug.pointer(row);
                }
                return Ok(Some(update));
            }

            let allowed = self.shared.core.config().codecs;
            let egfx = self.effective_egfx();
            match self.mode {
                Mode::Undecided => match codec::select(allowed, egfx) {
                    Decision::Wait => {
                        let deadline = self.grace_deadline;
                        self.wait(false, Some(deadline)).await;
                    }
                    Decision::Use(Transport::Egfx) => self.enter_egfx(),
                    Decision::Use(Transport::Legacy) => self.enter_legacy(),
                    Decision::Reject => {
                        info!(egfx = ?egfx, "no common codec with the client");
                        self.shared.request_disconnect("no common codec");
                        return Ok(None);
                    }
                },
                Mode::Legacy => {
                    if codec::select(allowed, egfx) == Decision::Use(Transport::Egfx) {
                        self.enter_egfx();
                        continue;
                    }
                    self.legacy_step().await;
                }
                Mode::Egfx => {
                    if codec::select(allowed, egfx) != Decision::Use(Transport::Egfx) {
                        match codec::select(allowed, EgfxStatus::Unavailable) {
                            Decision::Use(_) => self.enter_legacy(),
                            _ => {
                                self.shared
                                    .request_disconnect("EGFX channel closed and no fallback codec");
                                return Ok(None);
                            }
                        }
                        continue;
                    }
                    self.wait(false, None).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use vds_core::geom::Rect;

    use super::DisplayUpdates;

    #[test]
    fn bitmap_width_is_a_multiple_of_four_and_stays_inside() {
        assert_eq!(
            DisplayUpdates::align_width(Rect::new(5, 1, 3, 2), 100),
            [Rect::new(4, 1, 4, 2)]
        );
        assert_eq!(
            DisplayUpdates::align_width(Rect::new(97, 0, 3, 1), 100),
            [Rect::new(96, 0, 4, 1)]
        );
        // 1366-wide frame, full-width damage: two overlapping aligned rectangles.
        assert_eq!(
            DisplayUpdates::align_width(Rect::new(0, 0, 1366, 8), 1366),
            [Rect::new(0, 0, 1364, 8), Rect::new(1362, 0, 4, 8)]
        );
        assert!(DisplayUpdates::align_width(Rect::new(0, 0, 2, 2), 3).is_empty());
    }
}
