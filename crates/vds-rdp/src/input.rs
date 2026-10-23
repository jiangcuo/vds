//! Client input → [`ClientEvent`]s for the host.

use std::sync::Arc;

use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{KeyboardEvent, MouseButton as RdpButton, MouseEvent, RdpServerInputHandler};
use tracing::debug;
use vds_core::config::SasHotkey;
use vds_core::debug::InputRow;
use vds_core::display::HeadLayout;
use vds_core::event::{ClientEvent, MouseButton};

use crate::session::SessionShared;

/// Wheel units per notch (`WHEEL_DELTA`).
const WHEEL_DELTA: i32 = 120;
/// ainput carries the wheel in 16.16 fixed point: FreeRDP sends `WHEEL_DELTA << 16` per notch.
const AINPUT_WHEEL_SCALE: i32 = 0x10000;

/// Scancode of End (with the E0 prefix; without it, numpad 1).
const SC_END: u8 = 0x4f;
/// Scancode of Delete (with the E0 prefix; without it, the numpad decimal point).
const SC_DELETE: u8 = 0x53;
/// Scancode of the left Ctrl (the right one is E0 1D).
const SC_CTRL_L: u8 = 0x1d;
/// Scancode of the left Alt (the right one is E0 38 = AltGr).
const SC_ALT_L: u8 = 0x38;

/// Source of mouse events that did not arrive over `FreeRDP::Advanced::Input`.
///
/// IronRDP feeds `MouseEvent::Scroll` from ainput and everything else from the fast-path (or
/// the slow-path, which is indistinguishable here).
const FAST_PATH: &str = "fast-path";
/// Source of mouse events that arrived over the ainput DVC.
const AINPUT: &str = "ainput";

/// Names of the F1–F10 scancodes (F11 and F12 sit at 0x57 and 0x58).
const FUNCTION_KEYS: [&str; 10] = ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10"];

/// Name of a key that may be recorded in `input.csv`.
///
/// The input log may keep the scancode of keys whose identity is not the text the user types —
/// modifiers, editing and navigation keys, function keys and the lock keys. Every other key is
/// `char`: the file then says that something was typed, never what.
///
/// The E0 prefix (bit 7 of the QEMU keycode) is part of the identity: `0x53` is the numpad
/// decimal point, `E0 53` is Delete.
fn key_kind(scancode: u8, extended: bool) -> &'static str {
    if extended {
        match scancode {
            0x1c => "numpad-enter",
            0x1d => "ctrl",
            0x35 => "numpad-div",
            0x37 => "print-screen",
            0x38 => "alt",
            0x47 => "home",
            0x48 => "up",
            0x49 => "page-up",
            0x4b => "left",
            0x4d => "right",
            0x4f => "end",
            0x50 => "down",
            0x51 => "page-down",
            0x52 => "insert",
            0x53 => "delete",
            0x5b | 0x5c => "win",
            _ => "char",
        }
    } else {
        match scancode {
            0x01 => "esc",
            0x0e => "backspace",
            0x0f => "tab",
            0x1c => "enter",
            0x1d => "ctrl",
            0x2a | 0x36 => "shift",
            0x38 => "alt",
            0x3a => "caps-lock",
            0x3b..=0x44 => FUNCTION_KEYS[usize::from(scancode - 0x3b)],
            0x45 => "num-lock",
            0x46 => "scroll-lock",
            0x57 => "f11",
            0x58 => "f12",
            _ => "char",
        }
    }
}

/// `input.csv` row of a keyboard event; character keys carry no scancode.
fn key_row(scancode: u8, extended: bool, down: bool) -> InputRow {
    let kind = key_kind(scancode, extended);
    let named = kind != "char";
    InputRow {
        kind,
        source: "",
        scancode: named.then_some(scancode),
        extended: named.then_some(extended),
        down: Some(down),
        value: None,
    }
}

/// Index of a mouse button in `input.csv` (BTN_LEFT order, as in the RDP input PDU).
fn button_index(button: RdpButton) -> i64 {
    match button {
        RdpButton::Left => 0,
        RdpButton::Right => 1,
        RdpButton::Middle => 2,
        RdpButton::X1 => 3,
        RdpButton::X2 => 4,
        _ => -1,
    }
}

/// Wheel units not yet made up into whole notches.
///
/// Clients send the rotation of the wheel in units of [`WHEEL_DELTA`] per notch, and they
/// accumulate when the user spins fast, so one event may carry several notches. Units that
/// do not make up a notch (a high-resolution wheel, or an odd client) are carried over.
#[derive(Debug, Default)]
struct Wheel {
    horizontal: i32,
    vertical: i32,
}

impl Wheel {
    /// Adds client wheel units and returns the whole notches they make up.
    fn add(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        self.horizontal = self.horizontal.saturating_add(dx);
        self.vertical = self.vertical.saturating_add(dy);
        let notches = (self.horizontal / WHEEL_DELTA, self.vertical / WHEEL_DELTA);
        self.horizontal %= WHEEL_DELTA;
        self.vertical %= WHEEL_DELTA;
        notches
    }
}

/// The Ctrl+Alt+End → Ctrl+Alt+Del translation (`ServerConfig::sas_hotkey`).
///
/// Both Ctrls count, but only the left Alt: the right Alt arrives as E0 38 (AltGr), and a
/// client holding AltGr is typing a character, not asking for a secure attention sequence.
#[derive(Debug, Default)]
struct Sas {
    ctrl: bool,
    alt: bool,
    /// An End press was turned into Delete, so its release must be too — even if a modifier
    /// was let go in between, or the guest would be left with Delete held down.
    injected: bool,
}

impl Sas {
    /// Scancode to inject for a key event.
    fn key(&mut self, code: u8, extended: bool, down: bool, hotkey: SasHotkey) -> u8 {
        if code == SC_CTRL_L {
            self.ctrl = down;
        } else if code == SC_ALT_L && !extended {
            self.alt = down;
        }
        if hotkey == SasHotkey::Off || !extended || code != SC_END {
            return code;
        }
        if down {
            self.injected = self.ctrl && self.alt;
        }
        if !self.injected {
            return code;
        }
        if !down {
            self.injected = false;
        }
        SC_DELETE
    }
}

pub(crate) struct InputHandler {
    shared: Arc<SessionShared>,
    wheel: Wheel,
    sas: Sas,
}

impl InputHandler {
    pub(crate) fn new(shared: Arc<SessionShared>) -> Self {
        Self {
            shared,
            wheel: Wheel::default(),
            sas: Sas::default(),
        }
    }

    fn push(&self, ev: ClientEvent) {
        self.shared.core.push_event(ev);
    }

    /// Records a mouse event. Positions are never written: only the kind, the button or the
    /// wheel units (see `docs/spec/logging.md` section 2).
    fn note_mouse(&self, kind: &'static str, source: &'static str, value: Option<i64>, down: Option<bool>) {
        self.note(InputRow {
            kind,
            source,
            scancode: None,
            extended: None,
            down,
            value,
        });
    }

    /// Appends a row to `input.csv`; one atomic load when `debug-input` is off.
    fn note(&self, row: InputRow) {
        if !vds_core::debug::input_enabled() {
            return;
        }
        if let Some(debug) = &self.shared.debug {
            debug.input(row);
        }
    }

    fn button(&self, button: MouseButton, down: bool) {
        self.push(ClientEvent::Button { button, down });
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        let (horizontal, vertical) = self.wheel.add(dx, dy);
        if horizontal != 0 || vertical != 0 {
            self.push(ClientEvent::Wheel {
                dx: horizontal,
                dy: vertical,
            });
        }
    }
}

fn map_button(button: RdpButton) -> Option<MouseButton> {
    match button {
        RdpButton::Left => Some(MouseButton::Left),
        RdpButton::Right => Some(MouseButton::Right),
        RdpButton::Middle => Some(MouseButton::Middle),
        RdpButton::X1 => Some(MouseButton::Side),
        RdpButton::X2 => Some(MouseButton::Extra),
        _ => None,
    }
}

/// Maps a desktop position to a head (heads are laid out side by side).
pub(crate) fn map_to_head(layout: &[HeadLayout], x: u32, y: u32) -> Option<ClientEvent> {
    let head = layout
        .iter()
        .find(|l| x >= l.x && x < l.x + l.width)
        .or_else(|| layout.last().filter(|l| x >= l.x))
        .or_else(|| layout.first())?;
    let lx = x.saturating_sub(head.x).min(head.width.saturating_sub(1));
    let ly = y.saturating_sub(head.y).min(head.height.saturating_sub(1));
    Some(ClientEvent::PointerAbs {
        head: head.index,
        x: lx,
        y: ly,
        width: head.width,
        height: head.height,
    })
}

impl RdpServerInputHandler for InputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let ev = match event {
            KeyboardEvent::Pressed { code, extended } => {
                let code = self.sas.key(code, extended, true, self.shared.core.config().sas_hotkey);
                self.note(key_row(code, extended, true));
                ClientEvent::Key {
                    scancode: code,
                    extended,
                    down: true,
                }
            }
            KeyboardEvent::Released { code, extended } => {
                let code = self
                    .sas
                    .key(code, extended, false, self.shared.core.config().sas_hotkey);
                self.note(key_row(code, extended, false));
                ClientEvent::Key {
                    scancode: code,
                    extended,
                    down: false,
                }
            }
            KeyboardEvent::UnicodePressed(code) => {
                // QEMU cannot map Unicode to a guest scancode: the event is dropped, and the
                // row says only that a character key was pressed.
                self.note(InputRow {
                    kind: "char",
                    source: "unicode",
                    scancode: None,
                    extended: None,
                    down: Some(true),
                    value: None,
                });
                ClientEvent::Unicode { code, down: true }
            }
            KeyboardEvent::UnicodeReleased(code) => {
                self.note(InputRow {
                    kind: "char",
                    source: "unicode",
                    scancode: None,
                    extended: None,
                    down: Some(false),
                    value: None,
                });
                ClientEvent::Unicode { code, down: false }
            }
            KeyboardEvent::Synchronize(flags) => {
                let scroll = flags.contains(SynchronizeFlags::SCROLL_LOCK);
                let num = flags.contains(SynchronizeFlags::NUM_LOCK);
                let caps = flags.contains(SynchronizeFlags::CAPS_LOCK);
                self.note(InputRow {
                    kind: "lock-sync",
                    source: "",
                    scancode: None,
                    extended: None,
                    down: None,
                    value: Some(
                        i64::from(u8::from(scroll)) | i64::from(u8::from(num)) << 1 | i64::from(u8::from(caps)) << 2,
                    ),
                });
                ClientEvent::LockSync { scroll, num, caps }
            }
        };
        self.push(ev);
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                self.note_mouse("move", FAST_PATH, None, None);
                let layout = self.shared.core.display().layout();
                if let Some(ev) = map_to_head(&layout, u32::from(x), u32::from(y)) {
                    self.push(ev);
                }
            }
            MouseEvent::RelMove { x, y } => {
                self.note_mouse("rel", FAST_PATH, None, None);
                self.push(ClientEvent::PointerRel { dx: x, dy: y });
            }
            MouseEvent::Button { x, y, button, pressed } => {
                self.note_mouse("button", FAST_PATH, Some(button_index(button)), Some(pressed));
                // The button acts at the position carried by the event.
                let layout = self.shared.core.display().layout();
                if let Some(ev) = map_to_head(&layout, u32::from(x), u32::from(y)) {
                    self.push(ev);
                }
                if let Some(b) = map_button(button) {
                    self.button(b, pressed);
                }
            }
            MouseEvent::ButtonRel { x, y, button, pressed } => {
                self.note_mouse("button", FAST_PATH, Some(button_index(button)), Some(pressed));
                if x != 0 || y != 0 {
                    self.push(ClientEvent::PointerRel { dx: x, dy: y });
                }
                if let Some(b) = map_button(button) {
                    self.button(b, pressed);
                }
            }
            MouseEvent::HorizontalScroll { value } => {
                self.note_mouse("wheel-h", FAST_PATH, Some(i64::from(value)), None);
                self.wheel(i32::from(value), 0);
            }
            MouseEvent::VerticalScroll { value } => {
                self.note_mouse("wheel-v", FAST_PATH, Some(i64::from(value)), None);
                self.wheel(0, i32::from(value));
            }
            MouseEvent::Scroll { x, y } => {
                self.note_mouse("wheel", AINPUT, Some(i64::from(y)), None);
                if x != 0 {
                    self.note_mouse("wheel-h", AINPUT, Some(i64::from(x)), None);
                }
                self.wheel(x / AINPUT_WHEEL_SCALE, y / AINPUT_WHEEL_SCALE);
            }
            other => {
                self.note_mouse("other", FAST_PATH, None, None);
                debug!(event = ?other, "unhandled mouse event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use vds_core::config::SasHotkey;
    use vds_core::display::HeadLayout;
    use vds_core::event::ClientEvent;

    use super::{Sas, key_kind, key_row, map_to_head};

    fn layout() -> Vec<HeadLayout> {
        vec![
            HeadLayout {
                index: 0,
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            },
            HeadLayout {
                index: 1,
                x: 100,
                y: 0,
                width: 200,
                height: 80,
            },
        ]
    }

    #[test]
    fn maps_to_second_head() {
        assert_eq!(
            map_to_head(&layout(), 150, 70),
            Some(ClientEvent::PointerAbs {
                head: 1,
                x: 50,
                y: 70,
                width: 200,
                height: 80
            })
        );
    }

    #[test]
    fn clamps_inside_head() {
        assert_eq!(
            map_to_head(&layout(), 50, 70),
            Some(ClientEvent::PointerAbs {
                head: 0,
                x: 50,
                y: 49,
                width: 100,
                height: 50
            })
        );
        assert!(map_to_head(&[], 0, 0).is_none());
    }

    /// One notch is `WHEEL_DELTA` units and must produce exactly one step; a client that
    /// accumulates notches reports them as one event, and must not scroll twice for them.
    #[test]
    fn wheel_units_become_notches() {
        let mut w = super::Wheel::default();
        assert_eq!(w.add(0, 120), (0, 1));
        assert_eq!(w.add(0, -120), (0, -1));
        assert_eq!(w.add(120, 0), (1, 0));
        assert_eq!(w.add(0, 360), (0, 3));
        assert_eq!(w.add(0, 240), (0, 2));
        // Units below a notch are carried over, not rounded up.
        assert_eq!(w.add(0, 40), (0, 0));
        assert_eq!(w.add(0, 80), (0, 1));
        assert_eq!(w.add(0, 20), (0, 0));
        assert_eq!(w.add(0, -140), (0, -1));
    }

    /// `input.csv` may name modifiers and non-character keys, but a printable key must
    /// only ever appear as `char`, with no scancode to reverse into the typed text.
    #[test]
    fn character_keys_are_never_written_down() {
        // Number row, letters, space, and the punctuation right of them.
        for sc in [0x02_u8, 0x0d, 0x10, 0x1e, 0x30, 0x39, 0x33] {
            for extended in [false, true] {
                for down in [true, false] {
                    let row = key_row(sc, extended, down);
                    assert_eq!(row.kind, "char", "scancode {sc:#x}");
                    assert!(row.scancode.is_none(), "scancode {sc:#x} leaked");
                    assert!(row.extended.is_none(), "flags of {sc:#x} leaked");
                    assert_eq!(row.down, Some(down));
                }
            }
        }
    }

    /// The same scancode means different keys with and without the E0 prefix.
    #[test]
    fn extended_prefix_is_part_of_the_key_identity() {
        assert_eq!(key_kind(0x53, true), "delete");
        assert_eq!(key_kind(0x53, false), "char"); // numpad decimal point
        assert_eq!(key_kind(0x47, true), "home");
        assert_eq!(key_kind(0x47, false), "char"); // numpad 7
        assert_eq!(key_row(0x53, true, true).scancode, Some(0x53));
    }

    #[test]
    fn named_keys_keep_their_scancode() {
        for (sc, extended, kind) in [
            (0x1d, false, "ctrl"),
            (0x38, false, "alt"),
            (0x2a, false, "shift"),
            (0x3b, false, "f1"),
            (0x44, false, "f10"),
            (0x57, false, "f11"),
            (0x58, false, "f12"),
            (0x3a, false, "caps-lock"),
            (0x48, true, "up"),
            (0x5b, true, "win"),
            (0x1c, true, "numpad-enter"),
        ] {
            let row = key_row(sc, extended, true);
            assert_eq!(row.kind, kind, "scancode {sc:#x}");
            assert_eq!(row.scancode, Some(sc));
            assert_eq!(row.extended, Some(extended));
        }
    }

    /// Feeds a sequence of events through the translation, returning one scancode per event.
    fn sas_seq(hotkey: SasHotkey, events: &[(u8, bool, bool)]) -> Vec<u8> {
        let mut sas = Sas::default();
        events
            .iter()
            .map(|&(code, extended, down)| sas.key(code, extended, down, hotkey))
            .collect()
    }

    /// The client's Ctrl+Alt+End reaches the guest as Ctrl+Alt+Del, press and release
    /// alike — including when a modifier is let go before End.
    #[test]
    fn ctrl_alt_end_becomes_ctrl_alt_delete() {
        assert_eq!(
            sas_seq(
                SasHotkey::CtrlAltEnd,
                &[
                    (0x1d, false, true),
                    (0x38, false, true),
                    (0x4f, true, true),
                    (0x4f, true, false),
                    (0x38, false, false),
                    (0x1d, false, false),
                ]
            ),
            vec![0x1d, 0x38, 0x53, 0x53, 0x38, 0x1d]
        );
        // End released last: the injected Delete must still come up.
        assert_eq!(
            sas_seq(
                SasHotkey::CtrlAltEnd,
                &[
                    (0x1d, false, true),
                    (0x38, false, true),
                    (0x4f, true, true),
                    (0x1d, false, false),
                    (0x4f, true, false),
                ]
            ),
            vec![0x1d, 0x38, 0x53, 0x1d, 0x53]
        );
    }

    /// End on its own, or with only one of the two modifiers, is just End.
    #[test]
    fn end_needs_both_modifiers() {
        for events in [
            vec![(0x4f, true, true)],
            vec![(0x1d, false, true), (0x4f, true, true)],
            vec![(0x38, false, true), (0x4f, true, true)],
            vec![(0x2a, false, true), (0x4f, true, true)],
            // Numpad 1: the same scancode without the E0 prefix is a character key.
            vec![(0x1d, false, true), (0x38, false, true), (0x4f, false, true)],
            // AltGr (E0 38) is not the Alt of a secure attention sequence.
            vec![(0x1d, false, true), (0x38, true, true), (0x4f, true, true)],
        ] {
            let out = sas_seq(SasHotkey::CtrlAltEnd, &events);
            assert_eq!(*out.last().unwrap(), 0x4f, "{events:?}");
        }
        // ...but the right Ctrl (E0 1D) still counts.
        assert_eq!(
            sas_seq(
                SasHotkey::CtrlAltEnd,
                &[(0x1d, true, true), (0x38, false, true), (0x4f, true, true)]
            ),
            vec![0x1d, 0x38, 0x53]
        );
    }

    /// `sas-hotkey=off` injects what the client sent.
    #[test]
    fn sas_off_passes_the_keys_through() {
        assert_eq!(
            sas_seq(
                SasHotkey::Off,
                &[(0x1d, false, true), (0x38, false, true), (0x4f, true, true)]
            ),
            vec![0x1d, 0x38, 0x4f]
        );
    }

    /// A key that was not translated on the way down is not translated on the way up, and
    /// the translation does not stick to later presses.
    #[test]
    fn translation_tracks_the_press_it_belongs_to() {
        let hotkey = SasHotkey::CtrlAltEnd;
        assert_eq!(
            sas_seq(
                hotkey,
                &[
                    (0x1d, false, true),
                    (0x38, false, true),
                    (0x4f, true, true),  // translated
                    (0x4f, true, false), // ...and so is its release
                    (0x4f, true, true),  // again
                    (0x4f, true, false),
                ]
            ),
            vec![0x1d, 0x38, 0x53, 0x53, 0x53, 0x53]
        );
        // A plain End released after the modifiers went down stays End.
        assert_eq!(
            sas_seq(
                hotkey,
                &[
                    (0x4f, true, true),
                    (0x1d, false, true),
                    (0x38, false, true),
                    (0x4f, true, false)
                ]
            ),
            vec![0x4f, 0x1d, 0x38, 0x4f]
        );
    }
}
