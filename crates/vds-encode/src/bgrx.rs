//! BGRX staging pictures for encoders that take RGB and convert it themselves (VA-API).

use vds_core::display::{BYTES_PER_PIXEL, FrameRef};
use vds_core::geom::Rect;

use crate::error::{Error, Result};

/// A `width`x`height` BGRX picture, `width * 4` bytes per row.
///
/// Only the damaged rectangles are copied in; the rest keeps the previous picture, so the
/// buffer always holds the whole current screen.
#[derive(Debug, Clone)]
pub struct BgrxFrame {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl BgrxFrame {
    /// Allocates a black picture.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidSize(width, height));
        }
        Ok(Self {
            width,
            height,
            data: vec![0; width as usize * height as usize * BYTES_PER_PIXEL],
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    /// The pixels.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Copies `rect` of `src` (same geometry) into the picture; clipped to both.
    pub fn copy_rect(&mut self, src: &FrameRef<'_>, rect: Rect) -> Result<()> {
        let bounds = Rect::new(0, 0, src.width.min(self.width), src.height.min(self.height));
        let r = rect.intersect(&bounds);
        if r.is_empty() {
            return Ok(());
        }
        let stride = self.stride();
        let row = r.w as usize * BYTES_PER_PIXEL;
        let x0 = r.x as usize * BYTES_PER_PIXEL;
        for y in r.y as usize..r.bottom() as usize {
            let s = y * src.stride + x0;
            let d = y * stride + x0;
            let from = src.pixels.get(s..s + row).ok_or(Error::InvalidSize(src.width, src.height))?;
            let to = self
                .data
                .get_mut(d..d + row)
                .ok_or(Error::InvalidSize(self.width, self.height))?;
            to.copy_from_slice(from);
        }
        Ok(())
    }
}
