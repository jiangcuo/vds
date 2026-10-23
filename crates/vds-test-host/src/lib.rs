//! A synthetic host for libvds: draws an animated test pattern into the core's heads and
//! answers client events the way QEMU would (resize, clipboard). Used by tests, `vds-demo`
//! and `vds-bench`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, info};
use vds_core::Core;
use vds_core::display::{BYTES_PER_PIXEL, Head};
use vds_core::event::{ClientEvent, ClientState, ClipboardType};
use vds_core::geom::Rect;

/// Side of the moving square.
const SQUARE: u32 = 96;
/// Text returned when the client pastes from the guest.
pub const CLIPBOARD_TEXT: &str = "hello from the vds test host";

/// Counters of events received from clients.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EventStats {
    /// Key events.
    pub keys: u64,
    /// Pointer and button events.
    pub pointer: u64,
    /// Layout (resize) requests.
    pub layouts: u64,
    /// Clipboard events.
    pub clipboard: u64,
    /// Clients that reached the initialized state.
    pub initialized: u64,
    /// Clients that disconnected.
    pub disconnected: u64,
}

struct Painter {
    head: Arc<Head>,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    pos: (u32, u32),
    vel: (i32, i32),
    frame: u32,
}

impl Painter {
    fn new(head: Arc<Head>) -> Self {
        let (width, height) = head.size();
        let mut p = Self {
            head,
            width,
            height,
            pixels: Vec::new(),
            pos: (0, 0),
            vel: (7, 5),
            frame: 0,
        };
        p.reset();
        p
    }

    fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    fn background(x: u32, y: u32) -> [u8; 4] {
        // Gradient with fine text-like stripes, to exercise chroma subsampling.
        let stripe = if (y / 2).is_multiple_of(9) && x % 7 < 4 { 60 } else { 0 };
        [
            u8::try_from((x * 255 / 4096) % 256).unwrap_or(0).saturating_sub(stripe),
            u8::try_from((y * 255 / 2048) % 256).unwrap_or(0).saturating_sub(stripe),
            160u8.saturating_sub(stripe),
            0,
        ]
    }

    fn reset(&mut self) {
        let (w, h) = self.head.size();
        self.width = w;
        self.height = h;
        self.pixels = vec![0; w as usize * h as usize * BYTES_PER_PIXEL];
        for y in 0..h {
            for x in 0..w {
                self.put(x, y, Self::background(x, y));
            }
        }
        self.pos = (
            w / 3 % w.saturating_sub(SQUARE).max(1),
            h / 3 % h.saturating_sub(SQUARE).max(1),
        );
        let _ = self.head.submit(&self.pixels, self.stride(), &[Rect::new(0, 0, w, h)]);
    }

    fn put(&mut self, x: u32, y: u32, px: [u8; 4]) {
        let o = y as usize * self.stride() + x as usize * BYTES_PER_PIXEL;
        self.pixels[o..o + 4].copy_from_slice(&px);
    }

    fn step(&mut self) {
        if self.head.size() != (self.width, self.height) {
            self.reset();
            return;
        }
        let max_x = self.width.saturating_sub(SQUARE);
        let max_y = self.height.saturating_sub(SQUARE);
        let old = Rect::new(self.pos.0, self.pos.1, SQUARE.min(self.width), SQUARE.min(self.height));
        let nx = i64::from(self.pos.0) + i64::from(self.vel.0);
        let ny = i64::from(self.pos.1) + i64::from(self.vel.1);
        if nx < 0 || nx > i64::from(max_x) {
            self.vel.0 = -self.vel.0;
        }
        if ny < 0 || ny > i64::from(max_y) {
            self.vel.1 = -self.vel.1;
        }
        self.pos = (
            u32::try_from(nx.clamp(0, i64::from(max_x))).unwrap_or(0),
            u32::try_from(ny.clamp(0, i64::from(max_y))).unwrap_or(0),
        );
        self.frame = self.frame.wrapping_add(1);
        for y in old.y..old.bottom() {
            for x in old.x..old.right() {
                self.put(x, y, Self::background(x, y));
            }
        }
        let new = Rect::new(self.pos.0, self.pos.1, old.w, old.h);
        let shade = u8::try_from(self.frame % 200).unwrap_or(0) + 55;
        for y in new.y..new.bottom() {
            for x in new.x..new.right() {
                let border = x - new.x < 4 || y - new.y < 4 || new.right() - x <= 4 || new.bottom() - y <= 4;
                let px = if border {
                    [255, 255, 255, 0]
                } else {
                    [shade, 40, 255 - shade, 0]
                };
                self.put(x, y, px);
            }
        }
        let _ = self.head.submit(&self.pixels, self.stride(), &[old, new]);
    }
}

/// The synthetic host.
pub struct TestHost {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Mutex<EventStats>>,
}

impl core::fmt::Debug for TestHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestHost").field("stats", &*self.stats.lock()).finish()
    }
}

impl TestHost {
    /// Creates the heads (`sizes[i]` is head `i`) and starts animating at `fps`.
    pub fn start(core: Core, sizes: &[(u32, u32)], fps: u32) -> vds_core::Result<Self> {
        for (i, (w, h)) in sizes.iter().enumerate() {
            core.display().add_head(u32::try_from(i).unwrap_or(u32::MAX), *w, *h)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(EventStats::default()));
        let thread = {
            let stop = Arc::clone(&stop);
            let stats = Arc::clone(&stats);
            let interval = Duration::from_millis(1000 / u64::from(fps.max(1)));
            std::thread::Builder::new()
                .name(String::from("vds-test-host"))
                .spawn(move || {
                    let mut painters: Vec<Painter> = core.display().heads().into_iter().map(Painter::new).collect();
                    let mut next = Instant::now();
                    while !stop.load(Ordering::Acquire) {
                        core.drain_events(|ev| handle_event(&core, &stats, ev));
                        if core.display().wants_frame() {
                            for p in &mut painters {
                                p.step();
                            }
                        }
                        next += interval;
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        } else {
                            next = now;
                        }
                    }
                })
                .map_err(vds_core::Error::Io)?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
            stats,
        })
    }

    /// Event counters so far.
    pub fn stats(&self) -> EventStats {
        self.stats.lock().clone()
    }

    /// Stops the animation thread.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TestHost {
    fn drop(&mut self) {
        self.halt();
    }
}

fn handle_event(core: &Core, stats: &Mutex<EventStats>, ev: ClientEvent) {
    let mut s = stats.lock();
    match ev {
        ClientEvent::Key { .. } | ClientEvent::Unicode { .. } | ClientEvent::LockSync { .. } => s.keys += 1,
        ClientEvent::PointerAbs { .. }
        | ClientEvent::PointerRel { .. }
        | ClientEvent::Button { .. }
        | ClientEvent::Wheel { .. } => s.pointer += 1,
        ClientEvent::Layout(monitors) => {
            s.layouts += 1;
            for (i, m) in monitors.iter().enumerate() {
                if let Some(h) = core.display().head(u32::try_from(i).unwrap_or(u32::MAX)) {
                    debug!(head = i, w = m.width, h = m.height, "test host resizing head");
                    let _ = h.resize(m.width & !1, m.height & !1);
                }
            }
        }
        ClientEvent::ClipboardRequest(ClipboardType::Text) => {
            s.clipboard += 1;
            drop(s);
            core.clipboard_data(ClipboardType::Text, CLIPBOARD_TEXT.as_bytes().to_vec());
        }
        ClientEvent::ClipboardRequest(ClipboardType::Png)
        | ClientEvent::ClipboardGrab(_)
        | ClientEvent::ClipboardRelease
        | ClientEvent::ClipboardData(..) => {
            s.clipboard += 1;
        }
        ClientEvent::Client(info, state) => match state {
            ClientState::Connected => info!(sid = info.sid, "test host: client connected"),
            ClientState::Initialized => s.initialized += 1,
            ClientState::Disconnected => s.disconnected += 1,
        },
    }
}
