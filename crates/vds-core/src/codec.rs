//! Codec identifiers and the negotiation algorithm.

use core::fmt;

use crate::error::{Error, Result};

/// A graphics codec, in negotiation priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// EGFX H.264 4:2:0, encoded with openh264 (or VA-API).
    Avc420,
    /// RemoteFX (MS-RDPRFX) via surface bits.
    RemoteFx,
    /// Plain bitmap updates.
    Bitmap,
}

impl Codec {
    /// All codecs in negotiation priority order.
    pub const ALL: [Codec; 3] = [Codec::Avc420, Codec::RemoteFx, Codec::Bitmap];

    /// Name used in configuration, logs and QMP.
    pub const fn as_str(self) -> &'static str {
        match self {
            Codec::Avc420 => "avc420",
            Codec::RemoteFx => "remotefx",
            Codec::Bitmap => "bitmap",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Codec::Avc420 => 1,
            Codec::RemoteFx => 2,
            Codec::Bitmap => 4,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The set of codecs the administrator allows (`-rdp codecs=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecSet(u8);

impl CodecSet {
    /// Every codec allowed (default).
    pub const ALL: CodecSet = CodecSet(1 | 2 | 4);
    /// No codec allowed (invalid as a configuration).
    pub const EMPTY: CodecSet = CodecSet(0);

    /// Returns `true` if `codec` is allowed.
    pub const fn contains(self, codec: Codec) -> bool {
        self.0 & codec.bit() != 0
    }

    /// Allows `codec`.
    pub fn insert(&mut self, codec: Codec) {
        self.0 |= codec.bit();
    }

    /// Returns `true` if no codec is allowed.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Bit mask used by the C ABI (`1` = avc420, `2` = remotefx, `4` = bitmap).
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Builds a set from the C ABI bit mask; unknown bits are rejected.
    pub fn from_bits(bits: u32) -> Result<Self> {
        if bits & !u32::from(Self::ALL.0) != 0 {
            return Err(Error::InvalidConfig(format!("unknown codec bits {bits:#x}")));
        }
        let set = CodecSet(u8::try_from(bits).map_err(|_| Error::InvalidConfig(String::from("codec bits")))?);
        if set.is_empty() {
            return Err(Error::InvalidConfig(String::from("no codec allowed")));
        }
        Ok(set)
    }

    /// Parses a list such as `avc420:remotefx:bitmap` (`:` or `,` separated).
    pub fn parse(s: &str) -> Result<Self> {
        let mut set = CodecSet::EMPTY;
        for name in s.split([':', ',']).map(str::trim).filter(|n| !n.is_empty()) {
            let codec = Codec::ALL
                .into_iter()
                .find(|c| c.as_str() == name)
                .ok_or_else(|| Error::InvalidConfig(format!("unknown codec `{name}`")))?;
            set.insert(codec);
        }
        if set.is_empty() {
            return Err(Error::InvalidConfig(String::from("no codec allowed")));
        }
        Ok(set)
    }

    /// Allowed codecs in priority order.
    pub fn iter(self) -> impl Iterator<Item = Codec> {
        Codec::ALL.into_iter().filter(move |c| self.contains(*c))
    }
}

impl Default for CodecSet {
    fn default() -> Self {
        Self::ALL
    }
}

/// State of the client's EGFX (graphics pipeline) channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgfxStatus {
    /// Not yet known: the channel may still be opened.
    Pending,
    /// Capabilities were exchanged.
    Ready {
        /// The negotiated capability set enables AVC420.
        avc420: bool,
    },
    /// The client does not use EGFX (channel refused, closed, or grace period expired).
    Unavailable,
}

/// How graphics are sent to a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// EGFX dynamic channel with AVC420.
    Egfx,
    /// Fast-path updates. IronRDP's encoder picks RemoteFX when the client advertises it in
    /// its Bitmap Codecs capability (and we allowed it), plain bitmap otherwise.
    Legacy,
}

impl Transport {
    /// Name used in logs and QMP for the codec in use.
    pub const fn codec_name(self) -> &'static str {
        match self {
            Transport::Egfx => "avc420",
            Transport::Legacy => "legacy",
        }
    }
}

/// Result of [`select`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Not enough information yet; wait for the EGFX channel (bounded by the grace period).
    Wait,
    /// Use this transport.
    Use(Transport),
    /// No common codec: the connection must be refused.
    Reject,
}

/// Codec negotiation: EGFX AVC420, then RemoteFX, then bitmap, among the allowed codecs.
pub fn select(allowed: CodecSet, egfx: EgfxStatus) -> Decision {
    match egfx {
        EgfxStatus::Ready { avc420: true } if allowed.contains(Codec::Avc420) => Decision::Use(Transport::Egfx),
        EgfxStatus::Pending if allowed.contains(Codec::Avc420) => Decision::Wait,
        _ if allowed.contains(Codec::RemoteFx) || allowed.contains(Codec::Bitmap) => Decision::Use(Transport::Legacy),
        _ => Decision::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::{Codec, CodecSet, Decision, EgfxStatus, Transport, select};

    #[test]
    fn parse_codec_list() {
        let s = CodecSet::parse("avc420:bitmap").unwrap();
        assert!(s.contains(Codec::Avc420));
        assert!(!s.contains(Codec::RemoteFx));
        assert!(s.contains(Codec::Bitmap));
        assert_eq!(
            CodecSet::parse("remotefx,avc420").unwrap().iter().collect::<Vec<_>>(),
            [Codec::Avc420, Codec::RemoteFx]
        );
        assert!(CodecSet::parse("qoiz").is_err());
        assert!(CodecSet::parse("").is_err());
    }

    #[test]
    fn avc420_wins_when_available() {
        assert_eq!(
            select(CodecSet::ALL, EgfxStatus::Ready { avc420: true }),
            Decision::Use(Transport::Egfx)
        );
    }

    #[test]
    fn waits_for_egfx_only_if_avc420_allowed() {
        assert_eq!(select(CodecSet::ALL, EgfxStatus::Pending), Decision::Wait);
        let no_avc = CodecSet::parse("remotefx:bitmap").unwrap();
        assert_eq!(select(no_avc, EgfxStatus::Pending), Decision::Use(Transport::Legacy));
    }

    #[test]
    fn egfx_without_avc_falls_back_to_legacy() {
        assert_eq!(
            select(CodecSet::ALL, EgfxStatus::Ready { avc420: false }),
            Decision::Use(Transport::Legacy)
        );
        assert_eq!(
            select(CodecSet::ALL, EgfxStatus::Unavailable),
            Decision::Use(Transport::Legacy)
        );
    }

    #[test]
    fn avc420_only_rejects_clients_without_it() {
        let only = CodecSet::parse("avc420").unwrap();
        assert_eq!(select(only, EgfxStatus::Unavailable), Decision::Reject);
        assert_eq!(select(only, EgfxStatus::Ready { avc420: false }), Decision::Reject);
    }

    #[test]
    fn bits_roundtrip() {
        let s = CodecSet::parse("avc420:remotefx").unwrap();
        assert_eq!(CodecSet::from_bits(u32::from(s.bits())).unwrap(), s);
        assert!(CodecSet::from_bits(0).is_err());
        assert!(CodecSet::from_bits(8).is_err());
    }
}
