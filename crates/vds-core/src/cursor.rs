//! Guest mouse cursor shape and visibility.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{Error, Result};
use crate::signal::Signal;

/// Largest cursor dimension accepted (RDP large pointers go up to 384).
pub const MAX_CURSOR_DIMENSION: u32 = 384;

/// A cursor image: `width * height` pixels, 0xAARRGGBB (straight alpha), top-down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Hot spot X.
    pub hot_x: u32,
    /// Hot spot Y.
    pub hot_y: u32,
    /// Pixels.
    pub argb: Vec<u32>,
}

impl CursorImage {
    /// Validates and builds a cursor image.
    pub fn new(width: u32, height: u32, hot_x: u32, hot_y: u32, argb: Vec<u32>) -> Result<Self> {
        if width == 0 || height == 0 || width > MAX_CURSOR_DIMENSION || height > MAX_CURSOR_DIMENSION {
            return Err(Error::InvalidArgument(format!("cursor size {width}x{height}")));
        }
        if argb.len() != width as usize * height as usize {
            return Err(Error::InvalidArgument(format!("cursor data has {} pixels", argb.len())));
        }
        Ok(Self {
            width,
            height,
            hot_x: hot_x.min(width - 1),
            hot_y: hot_y.min(height - 1),
            argb,
        })
    }
}

/// Snapshot of the cursor state.
#[derive(Debug, Clone, Default)]
pub struct CursorState {
    /// Current shape; `None` before the guest defined one.
    pub shape: Option<Arc<CursorImage>>,
    /// Whether the cursor is visible.
    pub visible: bool,
    /// Incremented on every change.
    pub generation: u64,
}

/// Shared cursor state.
#[derive(Debug, Default)]
pub struct Cursor {
    state: Mutex<CursorState>,
    signal: Signal,
}

impl Cursor {
    /// Creates a hidden cursor without shape.
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines a new shape (makes the cursor visible).
    pub fn define(&self, image: CursorImage) {
        {
            let mut s = self.state.lock();
            s.shape = Some(Arc::new(image));
            s.visible = true;
            s.generation += 1;
        }
        self.signal.raise();
    }

    /// Shows or hides the cursor.
    pub fn set_visible(&self, visible: bool) {
        {
            let mut s = self.state.lock();
            if s.visible == visible {
                return;
            }
            s.visible = visible;
            s.generation += 1;
        }
        self.signal.raise();
    }

    /// Current state.
    pub fn snapshot(&self) -> CursorState {
        self.state.lock().clone()
    }

    /// Signal raised on every change.
    pub fn signal(&self) -> &Signal {
        &self.signal
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, CursorImage};

    #[test]
    fn rejects_bad_images() {
        assert!(CursorImage::new(0, 1, 0, 0, vec![]).is_err());
        assert!(CursorImage::new(2, 2, 0, 0, vec![0; 3]).is_err());
        let c = CursorImage::new(2, 2, 9, 9, vec![0; 4]).unwrap();
        assert_eq!((c.hot_x, c.hot_y), (1, 1));
    }

    #[test]
    fn generation_tracks_changes() {
        let c = Cursor::new();
        c.define(CursorImage::new(1, 1, 0, 0, vec![0]).unwrap());
        let g = c.snapshot().generation;
        c.set_visible(true);
        assert_eq!(c.snapshot().generation, g);
        c.set_visible(false);
        assert_eq!(c.snapshot().generation, g + 1);
    }
}
