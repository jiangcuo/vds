//! Events exchanged between the host (QEMU) and protocol sessions.

use std::net::SocketAddr;
use std::time::SystemTime;

/// Mouse buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
    /// Back / X1 button.
    Side,
    /// Forward / X2 button.
    Extra,
}

/// One monitor of a client-requested layout (MS-RDPEDISP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorLayout {
    /// Left edge on the client's desktop.
    pub left: i32,
    /// Top edge on the client's desktop.
    pub top: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Physical width in millimetres (0 if unknown).
    pub width_mm: u32,
    /// Physical height in millimetres (0 if unknown).
    pub height_mm: u32,
    /// Primary monitor.
    pub primary: bool,
}

/// Clipboard data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    /// UTF-8 text (QEMU `QEMU_CLIPBOARD_TYPE_TEXT`).
    Text,
    /// A PNG image (QEMU `QEMU_CLIPBOARD_TYPE_PNG`).
    Png,
}

impl ClipboardType {
    /// C ABI value.
    pub const fn to_raw(self) -> u32 {
        match self {
            ClipboardType::Text => 1,
            ClipboardType::Png => 2,
        }
    }

    /// Parses the C ABI value.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(ClipboardType::Text),
            2 => Some(ClipboardType::Png),
            _ => None,
        }
    }
}

/// Lifecycle states reported for a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// TCP connection accepted (QMP `RDP_CONNECTED`).
    Connected,
    /// Authentication and capability exchange finished (QMP `RDP_INITIALIZED`).
    Initialized,
    /// Connection closed (QMP `RDP_DISCONNECTED`).
    Disconnected,
}

/// Information about a client, used for QMP events and `query-rdp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Session id.
    pub sid: u64,
    /// Remote address, if known.
    pub peer: Option<SocketAddr>,
    /// Authenticated user name.
    pub user: Option<String>,
    /// Codec in use (`avc420`, `legacy`), once negotiated.
    pub codec: Option<&'static str>,
    /// Connection time.
    pub connected_at: SystemTime,
    /// Measured frames per second (last second).
    pub fps: u32,
    /// Measured video bitrate in kbit/s (last second).
    pub bitrate_kbps: u32,
    /// Round-trip time in milliseconds, if measured.
    pub rtt_ms: Option<u32>,
    /// Reason of the disconnection (only for [`ClientState::Disconnected`]).
    pub reason: Option<String>,
}

impl ClientInfo {
    /// Info for a freshly accepted connection.
    pub fn new(sid: u64, peer: Option<SocketAddr>) -> Self {
        Self {
            sid,
            peer,
            user: None,
            codec: None,
            connected_at: SystemTime::now(),
            fps: 0,
            bitrate_kbps: 0,
            rtt_ms: None,
            reason: None,
        }
    }
}

/// Events from a client towards the host, delivered by `Core::drain_events` on the host's
/// main thread.
///
/// Keyboard and clipboard payloads are sensitive: never log their content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// Key press or release, XT scancode set 1 (`extended` = 0xE0 prefix).
    Key {
        /// Scancode without prefix.
        scancode: u8,
        /// 0xE0-prefixed key.
        extended: bool,
        /// Pressed.
        down: bool,
    },
    /// Unicode key event (UTF-16 code unit).
    Unicode {
        /// UTF-16 code unit.
        code: u16,
        /// Pressed.
        down: bool,
    },
    /// Lock key state of the client (sent on focus).
    LockSync {
        /// Scroll lock.
        scroll: bool,
        /// Num lock.
        num: bool,
        /// Caps lock.
        caps: bool,
    },
    /// Absolute pointer position relative to `head`.
    PointerAbs {
        /// Head index.
        head: u32,
        /// X within the head.
        x: u32,
        /// Y within the head.
        y: u32,
        /// Head width (for scaling to absolute input devices).
        width: u32,
        /// Head height.
        height: u32,
    },
    /// Relative pointer motion.
    PointerRel {
        /// Delta X.
        dx: i32,
        /// Delta Y.
        dy: i32,
    },
    /// Mouse button.
    Button {
        /// Button.
        button: MouseButton,
        /// Pressed.
        down: bool,
    },
    /// Wheel notches: positive `dy` scrolls up, positive `dx` scrolls right.
    Wheel {
        /// Horizontal notches.
        dx: i32,
        /// Vertical notches.
        dy: i32,
    },
    /// Client requested a monitor layout (resize).
    Layout(Vec<MonitorLayout>),
    /// The client copied data of these types.
    ClipboardGrab(Vec<ClipboardType>),
    /// The client clipboard is empty.
    ClipboardRelease,
    /// The client wants the host clipboard content of this type; answer with
    /// `Core::clipboard_data`.
    ClipboardRequest(ClipboardType),
    /// Client clipboard content, in reply to `Core::clipboard_request`.
    ClipboardData(ClipboardType, Vec<u8>),
    /// Client state change (for QMP events).
    Client(ClientInfo, ClientState),
}

/// Events from the host towards the active session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostClipboardEvent {
    /// The guest copied data of these types.
    Grab(Vec<ClipboardType>),
    /// The guest clipboard is empty.
    Release,
    /// The guest wants the client's clipboard content of this type.
    Request(ClipboardType),
    /// Guest clipboard content, in reply to a [`ClientEvent::ClipboardRequest`].
    Data(ClipboardType, Vec<u8>),
}
