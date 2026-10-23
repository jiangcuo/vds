//! Clipboard image conversion between `CF_DIB` (MS-RDPECLIP 2.2.5.2.3, a packed DIB) and PNG.
//!
//! The host side of the clipboard (QEMU, spice-vdagent) speaks PNG; RDP clients always offer
//! and accept `CF_DIB`, and only some of them a registered "PNG" format.

use std::io::Cursor;

/// `BITMAPINFOHEADER` size.
const BIH_SIZE: usize = 40;
/// `biCompression`: uncompressed.
const BI_RGB: u32 = 0;
/// `biCompression`: uncompressed with colour masks.
const BI_BITFIELDS: u32 = 3;
/// Largest image converted, in pixels (8192 x 4096); bigger images are refused.
const MAX_PIXELS: u64 = 8192 * 4096;

/// Conversion failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageError {
    /// The DIB is truncated or uses a layout this module does not handle.
    #[error("unsupported or malformed DIB: {0}")]
    Dib(&'static str),
    /// The image is larger than `MAX_PIXELS`.
    #[error("image too large ({0}x{1})")]
    TooLarge(u32, u32),
    /// The PNG could not be decoded.
    #[error("PNG decoding failed: {0}")]
    Decode(#[from] png::DecodingError),
    /// The PNG could not be encoded.
    #[error("PNG encoding failed: {0}")]
    Encode(#[from] png::EncodingError),
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn check_size(w: u32, h: u32) -> Result<(), ImageError> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(ImageError::TooLarge(w, h));
    }
    Ok(())
}

/// One colour channel of a `BI_BITFIELDS` pixel.
#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        let shift = if mask == 0 { 0 } else { mask.trailing_zeros() };
        let bits = mask.count_ones();
        let max = if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 };
        Self { mask, shift, max }
    }

    fn get(self, px: u32) -> u8 {
        if self.mask == 0 || self.max == 0 {
            return 0;
        }
        let v = (px & self.mask) >> self.shift;
        if self.max == 255 {
            v as u8
        } else {
            ((u64::from(v) * 255 + u64::from(self.max) / 2) / u64::from(self.max)) as u8
        }
    }
}

/// Converts a packed DIB (`CF_DIB`, also the header + bits of `CF_DIBV5`) into a PNG.
///
/// Handles 24- and 32-bit `BI_RGB` and 16/32-bit `BI_BITFIELDS`, which is what Windows and
/// FreeRDP put on the clipboard. A 32-bit `BI_RGB` image whose alpha bytes are all zero is
/// treated as opaque (the usual case for `CF_DIB`).
pub(crate) fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, ImageError> {
    let hdr_size = u32_at(dib, 0).ok_or(ImageError::Dib("truncated header"))? as usize;
    if hdr_size < BIH_SIZE || hdr_size > dib.len() {
        return Err(ImageError::Dib("bad header size"));
    }
    let width = u32_at(dib, 4).ok_or(ImageError::Dib("truncated header"))? as i32;
    let height = u32_at(dib, 8).ok_or(ImageError::Dib("truncated header"))? as i32;
    let bpp = u16_at(dib, 14).ok_or(ImageError::Dib("truncated header"))?;
    let compression = u32_at(dib, 16).ok_or(ImageError::Dib("truncated header"))?;
    let clr_used = u32_at(dib, 32).ok_or(ImageError::Dib("truncated header"))? as usize;
    if width <= 0 || height == 0 {
        return Err(ImageError::Dib("bad dimensions"));
    }
    let (w, h) = (width as u32, height.unsigned_abs());
    check_size(w, h)?;
    let bottom_up = height > 0;

    // Colour masks: inside a V4/V5 header, or three DWORDs after a plain BITMAPINFOHEADER.
    let mut masks_end = hdr_size;
    let (r, g, b, a) = match (compression, bpp) {
        (BI_RGB, 24 | 32) => (
            0x00FF_0000,
            0x0000_FF00,
            0x0000_00FF,
            if bpp == 32 { 0xFF00_0000 } else { 0 },
        ),
        (BI_BITFIELDS, 16 | 32) => {
            let at = if hdr_size == BIH_SIZE {
                masks_end += 12;
                BIH_SIZE
            } else {
                40
            };
            let m = |i: usize| u32_at(dib, at + i * 4).ok_or(ImageError::Dib("truncated masks"));
            let alpha = if hdr_size >= 56 { m(3)? } else { 0 };
            (m(0)?, m(1)?, m(2)?, alpha)
        }
        _ => return Err(ImageError::Dib("unsupported bit depth or compression")),
    };
    let (cr, cg, cb, ca) = (Channel::new(r), Channel::new(g), Channel::new(b), Channel::new(a));

    let bits_off = masks_end + clr_used * 4;
    let stride = (w as usize * usize::from(bpp)).div_ceil(32) * 4;
    let bits = dib
        .get(bits_off..bits_off + stride * h as usize)
        .ok_or(ImageError::Dib("truncated pixel data"))?;

    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = if bottom_up { h as usize - 1 - y } else { y };
        let line = &bits[row * stride..row * stride + stride];
        for x in 0..w as usize {
            let px = match bpp {
                16 => u32::from(u16::from_le_bytes([line[x * 2], line[x * 2 + 1]])),
                24 => u32::from_le_bytes([line[x * 3], line[x * 3 + 1], line[x * 3 + 2], 0]),
                _ => u32::from_le_bytes([line[x * 4], line[x * 4 + 1], line[x * 4 + 2], line[x * 4 + 3]]),
            };
            rgba.extend_from_slice(&[
                cr.get(px),
                cg.get(px),
                cb.get(px),
                if a == 0 { 255 } else { ca.get(px) },
            ]);
        }
    }
    let has_alpha = a != 0 && rgba.chunks_exact(4).any(|p| p[3] != 0);
    if !has_alpha {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    encode_png(w, h, &rgba)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

/// Converts a PNG into a packed DIB (`CF_DIB`): `BITMAPINFOHEADER`, 32-bit `BI_RGB`, bottom-up.
pub(crate) fn png_to_dib(data: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_size(w, h)?;
    let size = reader.output_buffer_size().ok_or(ImageError::TooLarge(w, h))?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf)?;
    let channels = frame.color_type.samples();
    let line = frame.line_size;

    let stride = w as usize * 4;
    let mut dib = Vec::with_capacity(BIH_SIZE + stride * h as usize);
    dib.extend_from_slice(&(BIH_SIZE as u32).to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&BI_RGB.to_le_bytes());
    dib.extend_from_slice(&((stride * h as usize) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&[0u8; 16]); // resolution, colours used, colours important
    for y in (0..h as usize).rev() {
        let src = buf
            .get(y * line..y * line + w as usize * channels)
            .ok_or(ImageError::Dib("short PNG row"))?;
        for p in src.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                1 => (p[0], p[0], p[0], 255),
                2 => (p[0], p[0], p[0], p[1]),
                3 => (p[0], p[1], p[2], 255),
                _ => (p[0], p[1], p[2], p[3]),
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::{dib_to_png, png_to_dib};

    #[test]
    fn png_survives_a_dib_round_trip() {
        let rgba: Vec<u8> = (0..3 * 2).flat_map(|i| [i as u8 * 40, 255 - i as u8, 7, 255]).collect();
        let png = super::encode_png(3, 2, &rgba).expect("encode");
        let dib = png_to_dib(&png).expect("to dib");
        let back = dib_to_png(&dib).expect("to png");
        let mut reader = png::Decoder::new(std::io::Cursor::new(back)).read_info().expect("info");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("frame");
        assert_eq!(buf, rgba);
    }

    #[test]
    fn a_24_bit_dib_is_read_bottom_up() {
        // 1x2, bottom row red, top row blue; rows padded to 4 bytes.
        let mut dib = Vec::new();
        for v in [40u32, 1, 2] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let png = dib_to_png(&dib).expect("to png");
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().expect("info");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("frame");
        assert_eq!(buf, [0, 0, 255, 255, 255, 0, 0, 255]);
    }
}
