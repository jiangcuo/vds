//! Splits a head's refreshes into video and still areas.
//!
//! The EGFX pipeline sends both codecs out of one H.264 stream: the areas that keep changing
//! go through H.264 (AVC420), the ones that stop changing are re-sent with ClearCodec, which is
//! lossless, so text and user-interface elements reach the client as exact pixels instead of
//! H.264 output. Deciding which block is which is what this module does.
//!
//! Every head is divided into [`BLOCK`]-sized blocks. Each block remembers the last [`WINDOW`]
//! frames taken from the head as a bitmask and when it was last damaged, which is what the
//! classification is built on:
//!
//! * damaged in at least [`VIDEO_CHANGES`] of the last [`WINDOW`] frames → video, H.264;
//! * damaged less often, or not at all → still, ClearCodec.
//!
//! Counting changes over a window instead of consecutive ones is what keeps a video running
//! slower than the display refresh in the video set: at 60 Hz a 30 fps picture damages its
//! blocks on every other frame, so no run of consecutive damage would ever be long enough.
//!
//! A block the client only has as H.264 output stays marked `lossy`; once it has been quiet
//! for the settle time it is offered again as a ClearCodec tile. That replaces the quantizer
//! refinement frame (`refine-delay`) whenever the mix is enabled.

use std::time::Instant;

use vds_core::geom::{Rect, Region};

/// Side of one classification block, in pixels.
///
/// 64 is the granularity EGFX surfaces and RemoteFX tiles are aligned to, and what the Windows
/// server itself classifies at.
pub(crate) const BLOCK: u32 = 64;
/// Refreshes a block's damage history covers.
const WINDOW: u32 = 8;
/// A block damaged in at least this many of the last `WINDOW` refreshes is video.
const VIDEO_CHANGES: u32 = 3;
/// Blocks a planned tile may span along each axis.
///
/// The tiles are lossless: their cost grows with the pixels they cover when the area is busy
/// (four bytes of residual per pixel at worst) and stays at a few bytes when it is not, so a
/// large tile is free over flat background and expensive over a photograph. Two blocks each
/// way keeps one tile at most [`MAX_TILE_PIXELS`] pixels, which bounds what a single frame can
/// be made to carry; adjacent tiles of a flat area still cost a handful of bytes each.
const MAX_TILE_BLOCKS: u32 = 2;
/// Pixels one planned tile covers at most.
pub(crate) const MAX_TILE_PIXELS: u32 = (BLOCK * MAX_TILE_BLOCKS) * (BLOCK * MAX_TILE_BLOCKS);

/// What one block remembers.
#[derive(Debug, Clone, Copy, Default)]
struct Block {
    /// One bit per observed frame, newest in bit 0: did that frame damage this block?
    history: u8,
    /// Milliseconds since the map's epoch of the newest damage.
    damaged_ms: u32,
    /// The client's copy is H.264 output, not the exact pixels.
    lossy: bool,
    /// The frame that was observed last damaged this block.
    fresh: bool,
}

/// Damage history of one head.
pub(crate) struct BlockMap {
    cols: u32,
    rows: u32,
    blocks: Vec<Block>,
    epoch: Instant,
    /// When the history was last aged, in milliseconds since `epoch`.
    last_ms: u32,
    /// How long a block must be quiet before its lossless re-send, in milliseconds.
    settle_ms: u32,
    /// Length of one frame, in milliseconds; ages the history when frames are not taken.
    refresh_ms: u32,
    /// Scratch grid of the rectangle passes, kept between calls.
    covered: Vec<bool>,
}

impl BlockMap {
    /// Creates a map for a `width`×`height` head. `settle_ms` is how long a still block has to
    /// be quiet before it is re-sent losslessly, `refresh_ms` the display frame interval.
    pub(crate) fn new(width: u32, height: u32, settle_ms: u32, refresh_ms: u32) -> Self {
        let cols = width.div_ceil(BLOCK);
        let rows = height.div_ceil(BLOCK);
        Self {
            cols,
            rows,
            blocks: vec![Block::default(); (cols as usize) * (rows as usize)],
            epoch: Instant::now(),
            last_ms: 0,
            settle_ms,
            refresh_ms: refresh_ms.max(1),
            covered: Vec::new(),
        }
    }

    /// Ages the history and records what the observed frame damaged.
    ///
    /// Called once per frame taken from the head, before anything is encoded. At least one bit
    /// ages per call, and one more for every frame length that passed without a frame being
    /// taken — a head that stops changing takes no frames at all, and without that the changes
    /// before it went quiet would sit in the history forever and keep the block a video block.
    pub(crate) fn observe(&mut self, damage: &Region, now: Instant) {
        let ms = self.millis(now);
        let aged = ms.saturating_sub(self.last_ms) / self.refresh_ms;
        let shift = aged.clamp(1, WINDOW);
        self.last_ms = ms;
        for b in &mut self.blocks {
            // Through `u32` so that a shift of `WINDOW` is a defined clear, not an overflow.
            b.history = (u32::from(b.history) << shift) as u8;
            b.fresh = false;
        }
        for r in damage.rects() {
            let (c0, c1, r0, r1) = block_range(r, self.cols, self.rows);
            for row in r0..r1 {
                for col in c0..c1 {
                    let b = &mut self.blocks[(row * self.cols + col) as usize];
                    b.history |= 1;
                    b.damaged_ms = ms;
                    b.fresh = true;
                }
            }
        }
    }

    /// Whether a lossless re-send is owed for anything.
    ///
    /// This is what keeps frames coming while a picture that is already on the client is being
    /// made exact, at a moment when the host reports no damage at all.
    pub(crate) fn still_due(&self, now: Instant) -> bool {
        let ms = self.millis(now);
        let (settle, refresh, aged) = (self.settle_ms, self.refresh_ms, self.last_ms);
        self.blocks
            .iter()
            .any(|b| owes_lossless(settle, refresh, aged, b, ms))
    }

    /// Block-aligned rectangles to re-send losslessly in this frame, at most `max_tiles`.
    ///
    /// Fresh blocks come first: their pixels never reached the client exactly, whereas a
    /// settled block is only owed a redraw because H.264 approximated it.
    pub(crate) fn plan(&mut self, now: Instant, max_tiles: usize) -> Vec<Rect> {
        let ms = self.millis(now);
        let Self {
            cols,
            rows,
            blocks,
            last_ms,
            settle_ms,
            refresh_ms,
            covered,
            ..
        } = self;
        let (cols, rows) = (*cols, *rows);
        let (settle, refresh, aged) = (*settle_ms, *refresh_ms, *last_ms);
        let blocks: &[Block] = blocks;
        covered.clear();
        covered.resize(blocks.len(), false);

        let mut out = Vec::new();
        collect_rects(
            cols,
            rows,
            blocks,
            covered,
            |b| b.fresh && !video_at(refresh, aged, b, ms),
            &mut out,
            max_tiles,
        );
        collect_rects(
            cols,
            rows,
            blocks,
            covered,
            |b| owes_lossless(settle, refresh, aged, b, ms),
            &mut out,
            max_tiles,
        );
        out
    }

    /// The damaged blocks no lossless tile covers this frame, merged into as few rectangles as
    /// the grid allows: what the H.264 frame has to carry itself.
    ///
    /// A damaged block that no tile covers has no other way to reach the client, so it stays in
    /// the H.264 frame even when the classification calls it still — its tile comes in a later
    /// frame, once there is room for it. That is why the frame's regions are damage minus
    /// tiles and not the video blocks alone.
    pub(crate) fn avc_regions(&mut self, damage: &Region, tiles: &[Rect]) -> Region {
        let Self {
            cols,
            rows,
            covered,
            blocks,
            ..
        } = self;
        let (cols, rows) = (*cols, *rows);
        covered.clear();
        covered.resize(blocks.len(), false);
        for r in damage.rects() {
            let (c0, c1, r0, r1) = block_range(r, cols, rows);
            for row in r0..r1 {
                for col in c0..c1 {
                    covered[(row * cols + col) as usize] = true;
                }
            }
        }
        for t in tiles {
            let (c0, c1, r0, r1) = block_range(t, cols, rows);
            for row in r0..r1 {
                for col in c0..c1 {
                    covered[(row * cols + col) as usize] = false;
                }
            }
        }

        let mut out = Region::new();
        for row in 0..rows {
            for col in 0..cols {
                if !covered[(row * cols + col) as usize] {
                    continue;
                }
                let mut w = 1;
                while col + w < cols && covered[(row * cols + col + w) as usize] {
                    w += 1;
                }
                let mut h = 1;
                'down: while row + h < rows {
                    for x in col..col + w {
                        if !covered[((row + h) * cols + x) as usize] {
                            break 'down;
                        }
                    }
                    h += 1;
                }
                for y in row..row + h {
                    for x in col..col + w {
                        covered[(y * cols + x) as usize] = false;
                    }
                }
                out.add(Rect::new(col * BLOCK, row * BLOCK, w * BLOCK, h * BLOCK));
            }
        }
        out
    }

    /// Records that the tiles of a frame never reached the client, so their blocks come back
    /// for another try instead of being left stale until the host damages them again.
    pub(crate) fn fail_sent(&mut self, tiles: &[Rect]) {
        for t in tiles {
            self.for_each_block(t, |b| b.lossy = true);
        }
    }

    /// Records what a sent frame moved to the client.
    ///
    /// Everything the H.264 frame listed as a metablock region is an approximation from now
    /// on; the rectangles that travelled as lossless tiles are exact pixels and clear that
    /// mark again. A client copies a decoded tile onto its surface as it processes the PDU
    /// (MS-RDPEGFX 3.3.5.1), so where the two overlap the tile pixel is what remains — on the
    /// client and here alike.
    pub(crate) fn mark_sent(&mut self, avc: &Region, tiles: &[Rect]) {
        for r in avc.rects() {
            self.for_each_block(r, |b| b.lossy = true);
        }
        for t in tiles {
            self.for_each_block(t, |b| b.lossy = false);
        }
    }

    fn millis(&self, now: Instant) -> u32 {
        u32::try_from(now.saturating_duration_since(self.epoch).as_millis()).unwrap_or(u32::MAX)
    }

    fn for_each_block(&mut self, r: &Rect, mut f: impl FnMut(&mut Block)) {
        let (c0, c1, r0, r1) = block_range(r, self.cols, self.rows);
        for row in r0..r1 {
            for col in c0..c1 {
                f(&mut self.blocks[(row * self.cols + col) as usize]);
            }
        }
    }
}

/// Block columns and rows a pixel rectangle covers, clipped to the grid.
fn block_range(r: &Rect, cols: u32, rows: u32) -> (u32, u32, u32, u32) {
    if r.is_empty() {
        return (0, 0, 0, 0);
    }
    let c0 = (r.x / BLOCK).min(cols);
    let c1 = r.right().div_ceil(BLOCK).min(cols);
    let r0 = (r.y / BLOCK).min(rows);
    let r1 = r.bottom().div_ceil(BLOCK).min(rows);
    (c0, c1, r0, r1)
}

/// Whether a lossless re-send is owed for one block.
///
/// Whether the block was damaged in this very frame is deliberately not consulted here: the
/// first pass of [`BlockMap::plan`] takes those blocks anyway, and [`BlockMap::still_due`] has
/// to agree with what `plan` offers — otherwise a frame the client never received would leave
/// its blocks owed and nothing would ever ask for them again. `quiet` is what keeps a block
/// that just changed from being tiled before it has settled.
fn owes_lossless(settle_ms: u32, refresh_ms: u32, aged_ms: u32, b: &Block, ms: u32) -> bool {
    b.lossy && quiet(settle_ms, b, ms) && !video_at(refresh_ms, aged_ms, b, ms)
}

/// The block has not been damaged for the settle time.
fn quiet(settle_ms: u32, b: &Block, ms: u32) -> bool {
    ms.saturating_sub(b.damaged_ms) >= settle_ms
}

/// The block changed often enough to be video, counting the frames that merely passed without
/// one being taken as frames without damage.
///
/// `aged_ms` is when the history was last aged, so what it holds has to slide by the frames
/// that passed since. Without that, a head that goes completely static would keep every block
/// marked video — the history only ages when a frame is taken — and nothing that still owes the
/// client exact pixels would ever be considered due.
fn video_at(refresh_ms: u32, aged_ms: u32, b: &Block, ms: u32) -> bool {
    let idle = ms.saturating_sub(aged_ms) / refresh_ms;
    if idle >= WINDOW {
        return false;
    }
    (b.history >> idle).count_ones() >= VIDEO_CHANGES
}

/// Appends the rectangles made of blocks `want` accepts, in row-major order.
///
/// The grid is scanned top to bottom, left to right; a block that starts a rectangle extends
/// right for as long as the blocks beside it qualify and are free — up to [`MAX_TILE_BLOCKS`]
/// of them — then down while the whole span does. `covered` carries over between the two
/// passes of [`BlockMap::plan`], so neither produces a rectangle the other covered. At most
/// `max` rectangles are produced.
fn collect_rects(
    cols: u32,
    rows: u32,
    blocks: &[Block],
    covered: &mut [bool],
    want: impl Fn(&Block) -> bool,
    out: &mut Vec<Rect>,
    max: usize,
) {
    for row in 0..rows {
        for col in 0..cols {
            if out.len() == max {
                return;
            }
            let idx = (row * cols + col) as usize;
            if covered[idx] || !want(&blocks[idx]) {
                continue;
            }
            let mut w = 1;
            while col + w < cols && w < MAX_TILE_BLOCKS {
                let i = (row * cols + col + w) as usize;
                if covered[i] || !want(&blocks[i]) {
                    break;
                }
                w += 1;
            }
            let mut h = 1;
            'down: while row + h < rows && h < MAX_TILE_BLOCKS {
                for x in col..col + w {
                    let i = ((row + h) * cols + x) as usize;
                    if covered[i] || !want(&blocks[i]) {
                        break 'down;
                    }
                }
                h += 1;
            }
            for y in row..row + h {
                for x in col..col + w {
                    covered[(y * cols + x) as usize] = true;
                }
            }
            out.push(Rect::new(col * BLOCK, row * BLOCK, w * BLOCK, h * BLOCK));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{BLOCK, BlockMap, VIDEO_CHANGES, WINDOW};
    use vds_core::geom::{Rect, Region};

    /// One refresh interval, the value the maps under test are built with.
    const REFRESH_MS: u64 = 16;

    fn refresh() -> u32 {
        u32::try_from(REFRESH_MS).unwrap()
    }

    fn one(r: Rect) -> Region {
        Region::from_rect(r)
    }

    /// A clock that advances one refresh at a time unless told otherwise.
    struct Clock {
        now: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self { now: Instant::now() }
        }

        fn tick(&mut self) -> Instant {
            self.now += Duration::from_millis(REFRESH_MS);
            self.now
        }

        fn advance(&mut self, ms: u64) -> Instant {
            self.now += Duration::from_millis(ms);
            self.now
        }
    }

    /// A block changed once is still and is sent as a tile; changed on every frame, it becomes
    /// video and stops being a tile until it settles again.
    #[test]
    fn video_needs_a_repeated_change() {
        let mut map = BlockMap::new(256, 128, 200, refresh());
        let mut clock = Clock::new();
        let area = Rect::new(0, 0, BLOCK, BLOCK);

        let t = clock.tick();
        map.observe(&one(area), t);
        assert_eq!(map.plan(t, 8), vec![area], "one change is still");
        map.mark_sent(&one(area), &[area]);
        assert!(!map.still_due(clock.advance(400)), "the tile was exact");

        // Changed on every frame from here on: still still, then video.
        for changes in 1..=VIDEO_CHANGES {
            let t = clock.tick();
            map.observe(&one(area), t);
            let plan = map.plan(t, 8);
            if changes < VIDEO_CHANGES {
                assert_eq!(plan, vec![area], "{changes} changes are still still");
            } else {
                assert!(plan.is_empty(), "{changes} changes are video");
            }
            map.mark_sent(&one(area), &[]);
        }
        assert!(
            map.still_due(clock.advance(240)),
            "the settled video area is owed an exact redraw"
        );
    }

    /// Video stays video on alternate refreshes: a 30 fps picture at a 60 Hz refresh.
    #[test]
    fn alternating_damage_is_video() {
        let mut map = BlockMap::new(BLOCK, BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let area = Rect::new(0, 0, BLOCK, BLOCK);
        for _ in 0..WINDOW {
            let t = clock.tick();
            map.observe(&one(area), t);
            clock.tick();
        }
        assert!(map.plan(clock.now, 8).is_empty(), "half of the window is still video");
    }

    /// Blocks the client has lossily are offered again once they have been quiet for the
    /// settle time, and not before.
    #[test]
    fn settled_blocks_come_back_for_a_lossless_send() {
        let mut map = BlockMap::new(2 * BLOCK, BLOCK, 100, refresh());
        let mut clock = Clock::new();
        let both = Rect::new(0, 0, 2 * BLOCK, BLOCK);
        for _ in 0..VIDEO_CHANGES + 1 {
            let t = clock.tick();
            map.observe(&one(both), t);
            clock.tick();
        }
        map.mark_sent(&one(both), &[]);
        assert!(!map.still_due(clock.now), "the blocks are still video");
        assert!(!map.still_due(clock.advance(40)), "the settle time has not passed");

        let later = clock.advance(120);
        assert!(map.still_due(later));
        let plan = map.plan(later, 8);
        assert_eq!(plan, vec![both], "adjacent blocks share one tile");
        map.mark_sent(&Region::new(), &plan);
        assert!(!map.still_due(later), "the tiles made the client's copy exact");
    }

    /// The plan stays within its tile limit and never repeats a block.
    #[test]
    fn plan_is_limited_and_disjoint() {
        let mut map = BlockMap::new(8 * BLOCK, BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let mut damage = Region::new();
        for col in (0..8).step_by(2) {
            damage.add(Rect::new(col * BLOCK, 0, BLOCK, BLOCK));
        }
        let t = clock.tick();
        map.observe(&damage, t);
        let plan = map.plan(t, 3);
        assert_eq!(plan.len(), 3);
        for (i, r) in plan.iter().enumerate() {
            let col = u32::try_from(i).unwrap() * 2;
            assert_eq!(*r, Rect::new(col * BLOCK, 0, BLOCK, BLOCK));
            for other in &plan[i + 1..] {
                assert_eq!(r.intersect(other), Rect::default(), "{r:?} overlaps {other:?}");
            }
        }
    }

    /// Adjacent still blocks share one tile, up to the cap that bounds one frame's payload.
    #[test]
    fn adjacent_blocks_merge_up_to_the_tile_cap() {
        let mut map = BlockMap::new(4 * BLOCK, 2 * BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let t = clock.tick();
        map.observe(&one(Rect::new(0, 0, 4 * BLOCK, 2 * BLOCK)), t);
        assert_eq!(
            map.plan(t, 8),
            vec![Rect::new(0, 0, 2 * BLOCK, 2 * BLOCK), Rect::new(2 * BLOCK, 0, 2 * BLOCK, 2 * BLOCK)]
        );
    }

    /// Damage outside the map's grid (a frame queued before a resize) is ignored.
    #[test]
    fn out_of_range_damage_is_clipped() {
        let mut map = BlockMap::new(BLOCK, BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let t = clock.tick();
        map.observe(&one(Rect::new(BLOCK, BLOCK, 4 * BLOCK, 4 * BLOCK)), t);
        assert!(map.plan(t, 8).is_empty());
    }

    /// An idle head owes nothing while its blocks were never sent lossily.
    #[test]
    fn a_fresh_map_owes_nothing() {
        let map = BlockMap::new(4 * BLOCK, 4 * BLOCK, 200, refresh());
        assert!(!map.still_due(Instant::now() + Duration::from_secs(5)));
    }

    /// The H.264 frame carries the damage the tiles leave behind, and nothing else.
    #[test]
    fn avc_regions_are_the_damage_the_tiles_leave() {
        let mut map = BlockMap::new(4 * BLOCK, BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let damage = one(Rect::new(0, 0, 4 * BLOCK, BLOCK));
        let t = clock.tick();
        map.observe(&damage, t);

        let tiled = [Rect::new(0, 0, 2 * BLOCK, BLOCK)];
        assert_eq!(
            map.avc_regions(&damage, &tiled),
            one(Rect::new(2 * BLOCK, 0, 2 * BLOCK, BLOCK))
        );
        assert_eq!(map.avc_regions(&damage, &[]), damage, "no tiles, no removal");
        assert!(
            map.avc_regions(&damage, &[Rect::new(0, 0, 4 * BLOCK, BLOCK)]).is_empty(),
            "a frame whose damage is all tiles carries no H.264 regions"
        );
        assert!(
            map.avc_regions(&Region::new(), &tiled).is_empty(),
            "an undamaged block is not an H.264 region even when nothing tiles it"
        );
    }

    /// A tile that never reached the client leaves its blocks owed, so the next frame picks
    /// them up again.
    #[test]
    fn a_failed_tile_comes_back() {
        let mut map = BlockMap::new(BLOCK, BLOCK, 0, refresh());
        let mut clock = Clock::new();
        let area = Rect::new(0, 0, BLOCK, BLOCK);
        let t = clock.tick();
        map.observe(&one(area), t);
        map.mark_sent(&Region::new(), &[area]);
        assert!(!map.still_due(t), "the tile made the client's copy exact");

        map.mark_sent(&Region::new(), &[area]);
        map.fail_sent(&[area]);
        assert!(map.still_due(t), "the failed tile is owed again");
    }
}
