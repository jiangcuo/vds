//! Measures BGRX→I420 conversion and openh264 encoding for typical desktop workloads.
//!
//! ```text
//! vds-bench [--frames 120] [--threads 2] [--bitrate 20000] [--qp-max 30]
//! vds-bench debug [--records 200000]
//! ```
//! Scenarios per resolution:
//! - `typing`: a 64x16 area changes per frame (cursor/text);
//! - `window`: a 400x300 area moves (dragging a window);
//! - `scroll`: a full-width band crosses the screen;
//! - `video`: the whole frame changes.
//!
//! The quantizer distribution at the end of each line is the point of `--qp-max`: with a
//! ceiling, VBV answers a bitrate it cannot pay for by leaving the frame large, not by
//! blurring it. `--qp-max 51` is the uncapped behaviour to compare against,
//! and the effect only appears once `--bitrate` is a real constraint for the scene.
//!
//! The `debug` mode measures the record paths of the debug directory
//! (`docs/spec/logging.md` section 5) with it off and on, and counts the allocations of each.

#![expect(clippy::print_stdout, reason = "command-line tool output")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use vds_core::debug::{self, DebugFlags, FrameRow, InputRow, PointerRow, SessionDebug};
use vds_core::display::FrameRef;
use vds_core::geom::Rect;
use vds_encode::{EncoderConfig, I420Frame, OpenH264Encoder, RateControl};

/// Counts allocations while armed, so "the call sites allocate nothing" is measured, not claimed.
struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards to `System`, which allocates the same memory; the counter only
// touches two atomics. Nothing here depends on the layout being valid.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `ptr` and `layout` come from this allocator, which forwards to `System`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` come from this allocator, which forwards to `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measurement: nanoseconds per iteration and allocations made during it.
struct Measured {
    ns_each: f64,
    allocs: u64,
}

fn measure(records: u32, body: impl FnOnce()) -> Measured {
    ARMED.store(true, Ordering::SeqCst);
    let start = Instant::now();
    body();
    let elapsed = start.elapsed();
    let allocs = ALLOCS.swap(0, Ordering::SeqCst);
    ARMED.store(false, Ordering::SeqCst);
    Measured {
        ns_each: elapsed.as_secs_f64() * 1e9 / f64::from(records),
        allocs,
    }
}

/// The record call sites of a session (`vds-rdp`), in both their real shapes: the input path
/// checks one atomic load before touching anything, the others check an `Option`.
fn bench_debug(records: u32) -> Result<()> {
    let frame = FrameRow {
        head: 0,
        frame_id: 0,
        kind: "p",
        bytes: 512,
        qp: 23,
        regions: 1,
        region_area: 1280,
        age_us: 100,
        convert_us: 200,
        encode_us: 400,
        handoff_us: 50,
        in_flight: 1,
        still_tiles: 0,
        still_bytes: 0,
    };
    let pointer = PointerRow {
        kind: "transparent",
        width: 32,
        height: 32,
        hot_x: 0,
        hot_y: 0,
        pdu: "color",
        and_mask_len: 128,
        dropped: None,
    };
    let input = InputRow {
        kind: "char",
        source: "fast-path",
        scancode: None,
        extended: None,
        down: Some(true),
        value: None,
    };

    // Off: exactly what a session carries when `debug-dir` is not configured. `session` returns
    // `None` at run time, so the compiler cannot fold the guards away.
    let off_records = records;
    let off: Option<Arc<SessionDebug>> = debug::session(1);
    assert!(off.is_none(), "the debug directory was already configured");
    let idle = measure(off_records, || {
        for i in 0..off_records {
            if debug::input_enabled()
                && let Some(debug) = &off
            {
                debug.input(input);
            }
            if let Some(debug) = &off {
                debug.pointer(pointer);
                debug.frame_sent(frame);
            }
            std::hint::black_box(i);
        }
    });

    // On: the same call sites with a directory, so the difference is what the files cost.
    let dir = std::env::temp_dir().join(format!("vds-bench-debug-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    debug::configure(
        Some(&dir.display().to_string()),
        DebugFlags {
            input: true,
            video: true,
            pdu: false,
        },
        Some("bench"),
    )?;
    let on = debug::session(1).context("the debug directory was not created")?;
    let records = records.min(20_000);
    let busy = measure(records, || {
        for i in 0..records {
            if debug::input_enabled() {
                on.input(input);
            }
            let mut row = frame;
            row.frame_id = i;
            on.frame_sent(row);
            on.frame_acked(i);
            on.pointer(pointer);
            std::hint::black_box(i);
        }
    });
    on.finish();
    let written = dir_bytes(&dir);
    drop(on);
    let _ = std::fs::remove_dir_all(&dir);

    println!("debug off (no directory, {off_records} records)");
    println!("  {:>8.1} ns/record  {:>6} allocations", idle.ns_each, idle.allocs);
    println!("debug on  (frame+pointer+input, {records} records, each row flushed)");
    println!(
        "  {:>8.1} ns/record  {:>6} allocations  {written} bytes written",
        busy.ns_each, busy.allocs
    );
    Ok(())
}

/// Bytes of the files below `dir` (the debug directory nests one level per process).
///
/// The files are read, not just measured: Windows keeps a directory entry's size stale while a
/// writer still holds the file open, which is exactly the state being measured.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_bytes(&e.path()),
            _ => std::fs::read(e.path()).map_or(0, |data| data.len() as u64),
        })
        .sum()
}

struct Scenario {
    name: &'static str,
    damage: fn(u32, u32, u32) -> Rect,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario {
        name: "typing",
        damage: |i, w, h| Rect::new((i * 9) % (w - 64), (h / 2) % (h - 16), 64, 16),
    },
    Scenario {
        name: "window",
        damage: |i, w, h| Rect::new((i * 7) % (w - 400), (i * 5) % (h - 300), 400, 300),
    },
    Scenario {
        // Scrolling: a full-width band crosses the screen, and the moving content under the
        // cursor is what VBV finds hardest to pay for.
        name: "scroll",
        damage: |i, w, h| Rect::new(0, (i * 16) % (h.saturating_sub(256).max(1)), w, h.min(256)),
    },
    Scenario {
        name: "video",
        damage: |_, w, h| Rect::new(0, 0, w, h),
    },
];

fn paint(px: &mut [u8], w: u32, r: Rect, seed: u32) {
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            let o = (y as usize * w as usize + x as usize) * 4;
            let v = x
                .wrapping_mul(31)
                .wrapping_add(y.wrapping_mul(17))
                .wrapping_add(seed.wrapping_mul(13));
            px[o] = (v & 0xff) as u8;
            px[o + 1] = ((v >> 3) & 0xff) as u8;
            px[o + 2] = ((v >> 6) & 0xff) as u8;
        }
    }
}

/// What the command line sets, and what every row of the report shares.
struct Params {
    frames: u32,
    threads: u32,
    bitrate_kbps: u32,
    qp_max: u8,
}

fn bench(w: u32, h: u32, p: &Params, s: &Scenario) -> Result<()> {
    let (frames, threads, bitrate_kbps, qp_max) = (p.frames, p.threads, p.bitrate_kbps, p.qp_max);
    let mut px = vec![0u8; w as usize * h as usize * 4];
    paint(&mut px, w, Rect::new(0, 0, w, h), 0);
    let mut pic = I420Frame::new(w, h)?;
    let mut enc = OpenH264Encoder::open(&EncoderConfig {
        width: pic.width(),
        height: pic.height(),
        fps: 60,
        threads,
        rate: RateControl::capped(bitrate_kbps, 60, qp_max),
    })?;
    let bounds = Rect::new(0, 0, pic.width(), pic.height());
    {
        let src = FrameRef {
            width: w,
            height: h,
            stride: w as usize * 4,
            pixels: &px,
        };
        pic.convert(&src, Rect::new(0, 0, w, h))?;
    }
    enc.encode(&pic, true, None, 0)?;

    let (mut conv, mut encode, mut bytes, mut worst) = (Duration::ZERO, Duration::ZERO, 0usize, Duration::ZERO);
    let mut qps: Vec<f64> = Vec::with_capacity(frames as usize);
    for i in 1..=frames {
        let r = (s.damage)(i, w, h);
        paint(&mut px, w, r, i);
        let t0 = Instant::now();
        {
            let src = FrameRef {
                width: w,
                height: h,
                stride: w as usize * 4,
                pixels: &px,
            };
            pic.convert(&src, r.align_out(16, &bounds))?;
        }
        let t1 = Instant::now();
        let f = enc
            .encode(&pic, false, None, i64::from(i) * 16)?
            .context("openh264 produced no frame")?;
        let t2 = Instant::now();
        conv += t1 - t0;
        encode += t2 - t1;
        worst = worst.max(t2 - t0);
        bytes += f.data.len();
        qps.push(f.quality);
    }
    let n = f64::from(frames);
    let bytes_each = bytes as f64 / n;
    let kbps_at_30 = bytes_each * 8.0 * 30.0 / 1000.0;
    // Frame rate the budget can actually pay for. This is the column the quantizer ceiling is about: with a
    // quantizer ceiling the frames are larger than the budget, so the rate falls instead of
    // the picture (the loop then waits for acknowledgements rather than encoding more).
    let fps_at_budget = if bytes_each > 0.0 {
        (f64::from(bitrate_kbps) * 1000.0 / 8.0 / bytes_each).min(30.0)
    } else {
        30.0
    };
    qps.sort_by(f64::total_cmp);
    let pick = |q: f64| -> f64 {
        if qps.is_empty() {
            0.0
        } else {
            qps[((qps.len() - 1) as f64 * q) as usize]
        }
    };
    println!(
        "{w:>4}x{h:<4} {:<7} convert {:>6.2} ms  encode {:>6.2} ms  worst {:>6.2} ms  {:>7.0} B/frame  {:>7.0} kbit/s @30fps  {:>4.1} fps @budget  qp min/med/p90/max {:>5.1}/{:>5.1}/{:>5.1}/{:>5.1}",
        s.name,
        conv.as_secs_f64() * 1000.0 / n,
        encode.as_secs_f64() * 1000.0 / n,
        worst.as_secs_f64() * 1000.0,
        bytes_each,
        kbps_at_30,
        fps_at_budget,
        qps.first().copied().unwrap_or(0.0),
        pick(0.5),
        pick(0.9),
        qps.last().copied().unwrap_or(0.0),
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let get = |name: &str, default: u32| -> Result<u32> {
        match args.iter().position(|a| a == name) {
            Some(i) => Ok(args.get(i + 1).context("missing value")?.parse()?),
            None => Ok(default),
        }
    };
    if args.get(1).is_some_and(|a| a == "debug") {
        return bench_debug(get("--records", 200_000)?);
    }
    let frames = get("--frames", 120)?;
    let threads = get("--threads", 2)?;
    let qp_max = u8::try_from(get("--qp-max", 30)?).context("--qp-max out of range")?;
    if !(1..=51).contains(&qp_max) {
        bail!("--qp-max {qp_max} out of range 1..=51");
    }
    let bitrate_kbps = get("--bitrate", 20_000)?;
    println!("vds-bench: {frames} frames per scenario, {threads} openh264 threads, {bitrate_kbps} kbit/s, qp-max {qp_max}");
    let p = Params {
        frames,
        threads,
        bitrate_kbps,
        qp_max,
    };
    for (w, h) in [(1280, 720), (1920, 1080)] {
        for s in &SCENARIOS {
            bench(w, h, &p, s)?;
        }
    }
    Ok(())
}
