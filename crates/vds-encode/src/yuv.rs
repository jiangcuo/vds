//! BGRX to I420 conversion of damaged areas, SIMD-accelerated by the `yuv` crate
//! (yuvutils-rs: AVX2 / SSE4.1 / NEON / LoongArch, selected at run time).
//!
//! Full-range BT.709, the matrix RDP clients use to decode AVC420.
//!
//! Partial conversion: the full-width
//! band covering the damaged rows is converted into a scratch buffer, then only the damaged
//! rectangle is copied into the persistent planes. The result is byte-identical to a
//! full-frame conversion, so unchanged macroblocks stay identical and the encoder emits P-skip.

use vds_core::display::FrameRef;
use vds_core::geom::Rect;
use yuv::{BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

use crate::error::{Error, Result};

/// H.264 macroblock size; encoded pictures are padded to a multiple of it.
pub const MACROBLOCK: u32 = 16;

/// Rounds a source dimension up to the multiple of [`MACROBLOCK`] that [`I420Frame`] stores.
pub(crate) const fn padded(dim: u32) -> u32 {
    dim.div_ceil(MACROBLOCK) * MACROBLOCK
}

/// Reusable band buffers.
#[derive(Debug, Clone, Default)]
struct Scratch {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

/// A planar 4:2:0 picture whose dimensions are multiples of [`MACROBLOCK`].
#[derive(Debug, Clone)]
pub struct I420Frame {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    scratch: Scratch,
}

impl I420Frame {
    /// Allocates a black picture large enough for a `width`x`height` source.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidSize(width, height));
        }
        let width = padded(width);
        let height = padded(height);
        let luma = width as usize * height as usize;
        Ok(Self {
            width,
            height,
            y: vec![0; luma],
            u: vec![128; luma / 4],
            v: vec![128; luma / 4],
            scratch: Scratch::default(),
        })
    }

    /// Padded width.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Padded height.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Luma plane (stride = width).
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// Cb plane (stride = width / 2).
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// Cr plane (stride = width / 2).
    pub fn v(&self) -> &[u8] {
        &self.v
    }

    /// Luma stride in bytes.
    pub fn y_stride(&self) -> usize {
        self.width as usize
    }

    /// Chroma stride in bytes.
    pub fn uv_stride(&self) -> usize {
        self.width as usize / 2
    }

    /// Converts `rect` (source coordinates) of `src` into this picture.
    ///
    /// The rectangle is expanded to even coordinates (whole 2x2 chroma blocks) and clipped to
    /// the source. Pixels outside the source (macroblock padding) stay black.
    pub fn convert(&mut self, src: &FrameRef<'_>, rect: Rect) -> Result<()> {
        let src_w = src.width.min(self.width);
        let src_h = src.height.min(self.height);
        let bounds = Rect::new(0, 0, src_w, src_h);
        let r = rect.align_out(2, &bounds);
        if r.is_empty() {
            return Ok(());
        }
        // `align_out` starts on even coordinates; the end is even unless clipped by an odd
        // source edge.
        let (x0, y0, x1, y1) = (r.x as usize, r.y as usize, r.right() as usize, r.bottom() as usize);
        let w = src_w as usize;
        let cw = w.div_ceil(2);
        let rows = y1 - y0;
        let chroma_rows = rows.div_ceil(2);

        if x0 == 0 && x1 == w {
            // A full-width band is exactly what the scratch conversion below would produce:
            // convert it in place and skip the copy (3-4 ms per 1080p frame).
            return self.convert_band_in_place(src, y0, rows, chroma_rows);
        }

        let s = &mut self.scratch;
        s.y.resize(w * rows, 0);
        s.u.resize(cw * chroma_rows, 0);
        s.v.resize(cw * chroma_rows, 0);
        let band = src
            .pixels
            .get(y0 * src.stride..)
            .ok_or(Error::InvalidSize(src.width, src.height))?;
        let mut img = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(&mut s.y),
            y_stride: src_w,
            u_plane: BufferStoreMut::Borrowed(&mut s.u),
            u_stride: u32::try_from(cw).map_err(|_| Error::InvalidSize(src_w, src_h))?,
            v_plane: BufferStoreMut::Borrowed(&mut s.v),
            v_stride: u32::try_from(cw).map_err(|_| Error::InvalidSize(src_w, src_h))?,
            width: src_w,
            height: u32::try_from(rows).map_err(|_| Error::InvalidSize(src_w, src_h))?,
        };
        yuv::bgra_to_yuv420(
            &mut img,
            band,
            u32::try_from(src.stride).map_err(|_| Error::InvalidSize(src_w, src_h))?,
            YuvRange::Full,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .map_err(|e| Error::Convert(format!("{e:?}")))?;

        let y_stride = self.width as usize;
        let uv_stride = y_stride / 2;
        for row in 0..rows {
            let dst = (y0 + row) * y_stride;
            let src_off = row * w;
            self.y[dst + x0..dst + x1].copy_from_slice(&s.y[src_off + x0..src_off + x1]);
        }
        let (cx0, cx1) = (x0 / 2, x1.div_ceil(2));
        for row in 0..chroma_rows {
            let dst = (y0 / 2 + row) * uv_stride;
            let src_off = row * cw;
            self.u[dst + cx0..dst + cx1].copy_from_slice(&s.u[src_off + cx0..src_off + cx1]);
            self.v[dst + cx0..dst + cx1].copy_from_slice(&s.v[src_off + cx0..src_off + cx1]);
        }
        Ok(())
    }

    /// Converts the full-width rows `y0..y0 + rows` of `src` straight into the planes.
    /// `y0` is even.
    fn convert_band_in_place(&mut self, src: &FrameRef<'_>, y0: usize, rows: usize, chroma_rows: usize) -> Result<()> {
        let size_err = || Error::InvalidSize(src.width, src.height);
        let src_w = src.width.min(self.width);
        let y_stride = self.width as usize;
        let uv_stride = y_stride / 2;
        let band = src.pixels.get(y0 * src.stride..).ok_or_else(size_err)?;
        let y_plane = self
            .y
            .get_mut(y0 * y_stride..(y0 + rows) * y_stride)
            .ok_or_else(size_err)?;
        let c0 = y0 / 2 * uv_stride;
        let c1 = c0 + chroma_rows * uv_stride;
        let u_plane = self.u.get_mut(c0..c1).ok_or_else(size_err)?;
        let v_plane = self.v.get_mut(c0..c1).ok_or_else(size_err)?;
        let mut img = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y_plane),
            y_stride: u32::try_from(y_stride).map_err(|_| size_err())?,
            u_plane: BufferStoreMut::Borrowed(u_plane),
            u_stride: u32::try_from(uv_stride).map_err(|_| size_err())?,
            v_plane: BufferStoreMut::Borrowed(v_plane),
            v_stride: u32::try_from(uv_stride).map_err(|_| size_err())?,
            width: src_w,
            height: u32::try_from(rows).map_err(|_| size_err())?,
        };
        yuv::bgra_to_yuv420(
            &mut img,
            band,
            u32::try_from(src.stride).map_err(|_| size_err())?,
            YuvRange::Full,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .map_err(|e| Error::Convert(format!("{e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use vds_core::display::FrameRef;
    use vds_core::geom::Rect;

    use super::I420Frame;

    fn solid(w: u32, h: u32, bgrx: [u8; 4]) -> Vec<u8> {
        bgrx.iter().copied().cycle().take(w as usize * h as usize * 4).collect()
    }

    fn pattern(w: u32, h: u32) -> Vec<u8> {
        (0..w * h * 4)
            .map(|i| u8::try_from((i * 37 + i / 7) % 251).unwrap())
            .collect()
    }

    fn frame(w: u32, h: u32, px: &[u8]) -> FrameRef<'_> {
        FrameRef {
            width: w,
            height: h,
            stride: w as usize * 4,
            pixels: px,
        }
    }

    #[test]
    fn pads_to_macroblocks() {
        let f = I420Frame::new(1366, 768).unwrap();
        assert_eq!((f.width(), f.height()), (1376, 768));
        assert_eq!(f.uv_stride(), 688);
    }

    #[test]
    fn white_and_black() {
        let px = solid(16, 16, [255, 255, 255, 0]);
        let mut f = I420Frame::new(16, 16).unwrap();
        f.convert(&frame(16, 16, &px), Rect::new(0, 0, 16, 16)).unwrap();
        assert!(f.y()[0] >= 254);
        assert!((127..=129).contains(&f.u()[0]));
        assert!((127..=129).contains(&f.v()[0]));

        let px = solid(16, 16, [0, 0, 0, 0]);
        f.convert(&frame(16, 16, &px), Rect::new(0, 0, 16, 16)).unwrap();
        assert!(f.y()[0] <= 1);
    }

    #[test]
    fn pure_red_follows_bt709() {
        let px = solid(16, 16, [0, 0, 255, 0]);
        let mut f = I420Frame::new(16, 16).unwrap();
        f.convert(&frame(16, 16, &px), Rect::new(0, 0, 16, 16)).unwrap();
        // BT.709: Y = 0.2126 * 255.
        assert!((53..=55).contains(&f.y()[0]), "{}", f.y()[0]);
        assert!(f.v()[0] > 240);
        assert!(f.u()[0] < 128);
    }

    #[test]
    fn rect_conversion_matches_full_conversion() {
        let (w, h) = (100, 70);
        let px = pattern(w, h);
        let src = frame(w, h, &px);
        let mut full = I420Frame::new(w, h).unwrap();
        full.convert(&src, Rect::new(0, 0, w, h)).unwrap();

        let mut partial = I420Frame::new(w, h).unwrap();
        // Odd-sized pieces that together cover the whole frame.
        for r in [
            Rect::new(0, 0, 50, 31),
            Rect::new(51, 0, 49, 41),
            Rect::new(0, 29, 100, 41),
        ] {
            partial.convert(&src, r).unwrap();
        }
        assert_eq!(partial.y(), full.y());
        assert_eq!(partial.u(), full.u());
        assert_eq!(partial.v(), full.v());
    }

    #[test]
    fn only_damaged_area_changes() {
        let px = solid(32, 32, [255, 255, 255, 0]);
        let mut f = I420Frame::new(32, 32).unwrap();
        f.convert(&frame(32, 32, &px), Rect::new(16, 16, 16, 16)).unwrap();
        assert_eq!(f.y()[0], 0);
        assert_eq!(f.u()[0], 128);
        assert!(f.y()[16 * 32 + 16] >= 254);
    }

    #[test]
    fn odd_source_sizes_do_not_read_out_of_bounds() {
        let px = solid(33, 17, [10, 20, 30, 0]);
        let mut f = I420Frame::new(33, 17).unwrap();
        f.convert(&frame(33, 17, &px), Rect::new(0, 0, 33, 17)).unwrap();
        let stride = f.y_stride();
        assert_eq!(f.y()[16 * stride + 32], f.y()[0]);
        // Padding beyond the source stays black.
        assert_eq!(f.y()[33], 0);
    }

    #[test]
    fn short_source_buffer_is_an_error() {
        let px = vec![0u8; 16];
        let mut f = I420Frame::new(16, 16).unwrap();
        let src = FrameRef {
            width: 16,
            height: 16,
            stride: 64,
            pixels: &px,
        };
        assert!(f.convert(&src, Rect::new(0, 0, 16, 16)).is_err());
    }
}
