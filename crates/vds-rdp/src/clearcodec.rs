//! ClearCodec tiles: the lossless half of a mixed EGFX frame.
//!
//! ClearCodec (MS-RDPEGFX 2.2.4.1) is the lossless codec every EGFX version has to support, and
//! the one the Windows server uses for text and interface elements. Its bitmap stream is a
//! three-layer composite; this module writes the first layer alone, the residual
//! (2.2.4.1.1.1), a row-major run-length encoding of the BGR pixels.
//!
//! Bands and subcodecs pay off against the V-bar cache they reference, which has to be kept in
//! step with the client's across frames. That is worth its state for a whole screen of text and
//! not for the few blocks one frame's damage covers, so every tile here carries everything the
//! client needs: one PDU, no cache, nothing that depends on what was sent before. Tiles are
//! also always larger than the 1024 pixels the glyph cache covers, so the encoder never emits a
//! glyph index either.
//!
//! The run splitter is ported from `ironrdp-graphics` (`crates/ironrdp-graphics/src/
//! clearcodec/mod.rs`, function `bgra_to_run_segments`, at the IronRDP revision pinned in
//! `Cargo.lock`), adapted to read one rectangle of a strided frame instead of a linear buffer.
//! `ironrdp-graphics` is not a dependency of this crate and the port is ~30 lines; the
//! wire format helpers come from `ironrdp-pdu`, which is.

use ironrdp_pdu::codecs::clearcodec::{RgbRunSegment, encode_residual_layer};
use vds_core::display::BYTES_PER_PIXEL;
use vds_core::geom::Rect;

use crate::blocks::MAX_TILE_PIXELS;

/// Bytes a tile takes before its residual: `flags`, `seqNumber` and the three layer counts
/// (`residualByteCount`, `bandsByteCount`, `subcodecByteCount`).
pub(crate) const TILE_HEADER: usize = 2 + 12;

/// Encodes the lossless tiles of one head.
///
/// The only state is the next sequence number. MS-RDPEGFX 2.2.4.1.1 gives every ClearCodec
/// bitmap stream a one-byte `seqNumber`, and clients enforce it: FreeRDP refuses the whole
/// stream when the number is not exactly the previous one plus one, which fails the containing
/// frame. The counter therefore advances only for streams that reach the wire: [`encode`] takes
/// the number to write and leaves the counter alone, and the caller calls [`commit`] once the
/// frame carrying those tiles is queued.
///
/// [`encode`]: TileEncoder::encode
/// [`commit`]: TileEncoder::commit
#[derive(Debug, Default)]
pub(crate) struct TileEncoder {
    seq: u8,
}

impl TileEncoder {
    /// Creates an encoder whose first tile carries sequence number 0.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The sequence number the next encoded tile must carry.
    pub(crate) fn next_seq(&self) -> u8 {
        self.seq
    }

    /// Advances past `tiles` streams that were sent, wrapping like the wire byte does.
    pub(crate) fn commit(&mut self, tiles: usize) {
        self.seq = self.seq.wrapping_add(u8::try_from(tiles % 256).unwrap_or(0));
    }

    /// Encodes `rect` of a BGRX frame (`stride` bytes per row) as a ClearCodec bitmap stream
    /// carrying sequence number `seq`.
    ///
    /// `rect` must be inside the frame and must not be empty; the caller clips it to the head
    /// and to a tile size, so the residual always fits the wire's 32-bit byte count.
    pub(crate) fn encode(&self, pixels: &[u8], stride: usize, rect: Rect, seq: u8) -> Vec<u8> {
        // The planner never offers a larger rectangle; one this size is already 4 bytes per
        // pixel at worst, and a frame carries several of them.
        debug_assert!(
            rect.area() <= u64::from(MAX_TILE_PIXELS),
            "tile of {} pixels above the planner's cap",
            rect.area()
        );
        let residual = encode_residual_layer(&run_segments(pixels, stride, rect));

        let mut out = Vec::with_capacity(TILE_HEADER + residual.len());
        // Flags 0: no glyph index, no cached glyph, no cache reset. A tile below the 1024
        // pixels the glyph cache covers never reaches this encoder (`MAX_TILE_PIXELS`).
        out.push(0);
        out.push(seq);
        out.extend_from_slice(&u32::try_from(residual.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // bandsByteCount: no V-bar cache
        out.extend_from_slice(&0u32.to_le_bytes()); // subcodecByteCount: no subcodec
        out.extend_from_slice(&residual);
        out
    }
}

/// Splits the pixels of `rect` into runs of one colour, in the order the decoder writes them.
///
/// A run may cross from the end of one row of the rectangle into the start of the next: the
/// client lays the residual out linearly over the destination rectangle, so a solid tile is
/// one segment whether or not the frame's rows are contiguous.
fn run_segments(pixels: &[u8], stride: usize, rect: Rect) -> Vec<RgbRunSegment> {
    let mut out = Vec::new();
    let mut run: Option<RgbRunSegment> = None;
    for y in rect.y..rect.bottom() {
        let row = y as usize * stride;
        for x in rect.x..rect.right() {
            let offset = row + x as usize * BYTES_PER_PIXEL;
            let (blue, green, red) = (pixels[offset], pixels[offset + 1], pixels[offset + 2]);
            match &mut run {
                Some(seg) if seg.blue == blue && seg.green == green && seg.red == red => {
                    seg.run_length += 1;
                }
                _ => {
                    if let Some(seg) = run.take() {
                        out.push(seg);
                    }
                    run = Some(RgbRunSegment {
                        blue,
                        green,
                        red,
                        run_length: 1,
                    });
                }
            }
        }
    }
    if let Some(seg) = run {
        out.push(seg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_TILE_PIXELS, TILE_HEADER, TileEncoder, run_segments};
    use ironrdp_pdu::codecs::clearcodec::decode_residual_layer;
    use vds_core::display::BYTES_PER_PIXEL;
    use vds_core::geom::Rect;

    /// A frame of `w`×`h` whose rows start at `stride`; `f` gives the colour of one pixel.
    fn frame(w: u32, h: u32, stride: usize, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut pixels = vec![0u8; stride * h as usize];
        for y in 0..h {
            for x in 0..w {
                let o = y as usize * stride + x as usize * BYTES_PER_PIXEL;
                let [b, g, r] = f(x, y);
                pixels[o..o + 4].copy_from_slice(&[b, g, r, 0xFF]);
            }
        }
        pixels
    }

    /// Expands a bitmap stream the way the client does: flags, sequence number, the three
    /// layer counts, then the runs written linearly over the destination rectangle.
    fn decode(stream: &[u8], width: u32, height: u32) -> (u8, Vec<[u8; 3]>) {
        assert_eq!(stream[0], 0, "no flags are expected in a tile");
        let seq = stream[1];
        let len = u32::from_le_bytes(stream[2..6].try_into().unwrap()) as usize;
        assert_eq!(u32::from_le_bytes(stream[6..10].try_into().unwrap()), 0, "bands");
        assert_eq!(u32::from_le_bytes(stream[10..14].try_into().unwrap()), 0, "subcodec");
        assert_eq!(stream.len(), TILE_HEADER + len, "residual length in the header");
        let mut pixels = Vec::new();
        for seg in decode_residual_layer(&stream[TILE_HEADER..]).unwrap() {
            for _ in 0..seg.run_length {
                pixels.push([seg.blue, seg.green, seg.red]);
            }
        }
        assert_eq!(pixels.len(), (width * height) as usize, "the tile fills the rectangle");
        (seq, pixels)
    }

    /// A run that spans row boundaries: the whole rectangle is one segment, and the stride of
    /// the frame is not what the client sees.
    #[test]
    fn a_solid_tile_is_one_segment() {
        let pixels = frame(2, 2, 16, |_, _| [0x11, 0x22, 0x33]);
        let enc = TileEncoder::new();
        let stream = enc.encode(&pixels, 16, Rect::new(0, 0, 2, 2), 0);
        // flags, seq, residualByteCount=4, bands=0, subcodec=0, then B, G, R, run=4.
        assert_eq!(
            stream,
            vec![0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x22, 0x33, 4]
        );
    }

    /// A tile inside a larger frame reads exactly its own rows and columns.
    #[test]
    fn a_tile_reads_only_its_rectangle() {
        let pixels = frame(6, 3, 6 * BYTES_PER_PIXEL, |x, y| {
            u8::try_from(x * 10 + y).map_or([0, 0, 0], |v| [v, 0, 0])
        });
        let rect = Rect::new(2, 1, 3, 2);
        let (_, decoded) = decode(
            &TileEncoder::new().encode(&pixels, 6 * BYTES_PER_PIXEL, rect, 0),
            rect.w,
            rect.h,
        );
        let expected: Vec<[u8; 3]> = [(2, 1), (3, 1), (4, 1), (2, 2), (3, 2), (4, 2)]
            .into_iter()
            .map(|(x, y)| [u8::try_from(x * 10 + y).unwrap(), 0, 0])
            .collect();
        assert_eq!(decoded, expected);
    }

    /// Every pixel distinct: one segment each, and the sequence number walks once committed.
    #[test]
    fn sequence_numbers_advance() {
        let pixels = frame(2, 1, 8, |x, _| [x as u8, 1, 2]);
        let rect = Rect::new(0, 0, 2, 1);
        let mut enc = TileEncoder::new();
        let first = enc.encode(&pixels, 8, rect, enc.next_seq());
        enc.commit(1);
        let second = enc.encode(&pixels, 8, rect, enc.next_seq());
        assert_eq!(run_segments(&pixels, 8, rect).len(), 2);
        assert_eq!(decode(&first, 2, 1).0, 0);
        assert_eq!(decode(&second, 2, 1).0, 1);
    }

    /// An encoded tile that never reaches the wire must not consume a sequence number: the
    /// client rejects the stream (FreeRDP `clear.c` fails the whole decode) when one is skipped.
    #[test]
    fn discarded_tiles_do_not_consume_sequence_numbers() {
        let pixels = frame(2, 1, 8, |x, _| [x as u8, 1, 2]);
        let rect = Rect::new(0, 0, 2, 1);
        let mut enc = TileEncoder::new();

        // Encoded, then dropped for the budget or because the frame was never sent.
        let dropped = enc.encode(&pixels, 8, rect, enc.next_seq());
        assert_eq!(decode(&dropped, 2, 1).0, 0);

        // The next tile the client actually receives carries the same number.
        let sent = enc.encode(&pixels, 8, rect, enc.next_seq());
        assert_eq!(decode(&sent, 2, 1).0, 0);
        enc.commit(1);
        assert_eq!(enc.next_seq(), 1);
    }

    /// The largest tile the planner produces stays inside the wire's 32-bit residual count.
    #[test]
    fn the_largest_tile_fits_the_header() {
        // Worst case: every pixel different, four bytes of residual each.
        let side = MAX_TILE_PIXELS.isqrt();
        let (w, h) = (side, MAX_TILE_PIXELS / side);
        let pixels = frame(w, h, 4 * w as usize, |x, y| {
            [u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 0x5A]
        });
        let stream = TileEncoder::new().encode(&pixels, 4 * w as usize, Rect::new(0, 0, w, h), 0);
        assert_eq!(stream.len(), TILE_HEADER + 4 * MAX_TILE_PIXELS as usize);
        let (_, decoded) = decode(&stream, w, h);
        assert_eq!(decoded.len(), MAX_TILE_PIXELS as usize);
    }
}
