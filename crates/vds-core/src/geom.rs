//! Rectangles and damage regions.

/// An axis-aligned rectangle in pixels. `w`/`h` may be zero (empty rectangle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

impl Rect {
    /// Creates a rectangle.
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Returns `true` if the rectangle covers no pixel.
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Exclusive right edge.
    pub const fn right(&self) -> u32 {
        self.x.saturating_add(self.w)
    }

    /// Exclusive bottom edge.
    pub const fn bottom(&self) -> u32 {
        self.y.saturating_add(self.h)
    }

    /// Number of pixels covered.
    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    /// Intersection with `other`, empty if they do not overlap.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            Rect::default()
        } else {
            Rect::new(x0, y0, x1 - x0, y1 - y0)
        }
    }

    /// Smallest rectangle containing both `self` and `other`.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Expands the rectangle outwards to multiples of `align` (e.g. 16 for H.264 macroblocks),
    /// clamped to `bounds`.
    pub fn align_out(&self, align: u32, bounds: &Rect) -> Rect {
        if self.is_empty() || align == 0 {
            return *self;
        }
        let x0 = self.x / align * align;
        let y0 = self.y / align * align;
        let x1 = self.right().div_ceil(align).saturating_mul(align);
        let y1 = self.bottom().div_ceil(align).saturating_mul(align);
        Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(bounds)
    }

    /// Returns `true` if `other` overlaps or touches `self`.
    fn touches(&self, other: &Rect) -> bool {
        self.x <= other.right() && other.x <= self.right() && self.y <= other.bottom() && other.y <= self.bottom()
    }
}

/// Maximum number of rectangles kept before a region collapses to its bounding box.
const MAX_RECTS: usize = 16;

/// A set of damaged rectangles.
///
/// Touching rectangles are merged; when more than `MAX_RECTS` (16) disjoint rectangles
/// accumulate the region degrades to its bounding box, which keeps per-frame work bounded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Region {
    rects: Vec<Rect>,
}

impl Region {
    /// Creates an empty region.
    pub const fn new() -> Self {
        Self { rects: Vec::new() }
    }

    /// Creates a region from a single rectangle.
    pub fn from_rect(rect: Rect) -> Self {
        let mut r = Self::new();
        r.add(rect);
        r
    }

    /// Returns `true` if the region is empty.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The rectangles of the region.
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// Adds a rectangle to the region.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        let mut merged = rect;
        // Repeatedly absorb touching rectangles; the list is short (<= MAX_RECTS).
        loop {
            let before = self.rects.len();
            self.rects.retain(|r| {
                if r.touches(&merged) {
                    merged = merged.union(r);
                    false
                } else {
                    true
                }
            });
            if self.rects.len() == before {
                break;
            }
        }
        self.rects.push(merged);
        if self.rects.len() > MAX_RECTS {
            let bbox = self.bounding_box();
            self.rects.clear();
            self.rects.push(bbox);
        }
    }

    /// Adds every rectangle of `other`.
    pub fn add_region(&mut self, other: &Region) {
        for r in &other.rects {
            self.add(*r);
        }
    }

    /// Smallest rectangle containing the whole region.
    pub fn bounding_box(&self) -> Rect {
        self.rects.iter().fold(Rect::default(), |acc, r| acc.union(r))
    }

    /// Total covered area (rectangles never overlap after merging).
    pub fn area(&self) -> u64 {
        self.rects.iter().map(Rect::area).sum()
    }

    /// Clips every rectangle to `bounds`.
    pub fn clip(&mut self, bounds: &Rect) {
        let rects = core::mem::take(&mut self.rects);
        for r in rects {
            self.add(r.intersect(bounds));
        }
    }

    /// Takes the content of the region, leaving it empty.
    pub fn take(&mut self) -> Region {
        core::mem::take(self)
    }

    /// Empties the region.
    pub fn clear(&mut self) {
        self.rects.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{Rect, Region};

    #[test]
    fn touching_rects_merge() {
        let mut r = Region::new();
        r.add(Rect::new(0, 0, 10, 10));
        r.add(Rect::new(10, 0, 10, 10));
        assert_eq!(r.rects(), &[Rect::new(0, 0, 20, 10)]);
    }

    #[test]
    fn disjoint_rects_stay_separate() {
        let mut r = Region::new();
        r.add(Rect::new(0, 0, 10, 10));
        r.add(Rect::new(100, 100, 10, 10));
        assert_eq!(r.rects().len(), 2);
        assert_eq!(r.bounding_box(), Rect::new(0, 0, 110, 110));
    }

    #[test]
    fn too_many_rects_collapse() {
        let mut r = Region::new();
        for i in 0..17 {
            r.add(Rect::new(i * 20, i * 20, 5, 5));
        }
        // The 17th disjoint rectangle exceeds the limit: everything collapses to the bbox.
        assert_eq!(r.rects(), &[Rect::new(0, 0, 325, 325)]);
    }

    #[test]
    fn align_out_clamps_to_bounds() {
        let bounds = Rect::new(0, 0, 100, 50);
        assert_eq!(Rect::new(17, 3, 2, 2).align_out(16, &bounds), Rect::new(16, 0, 16, 16));
        assert_eq!(
            Rect::new(90, 40, 10, 10).align_out(16, &bounds),
            Rect::new(80, 32, 20, 18)
        );
    }

    #[test]
    fn empty_rect_is_ignored() {
        let mut r = Region::new();
        r.add(Rect::new(5, 5, 0, 10));
        assert!(r.is_empty());
    }
}
