//! Colors of the WrongIndex window.

pub const BG: u32 = 0x10151b;
pub const SIDEBAR: u32 = 0x121922;
pub const PANEL: u32 = 0x171e27;
pub const RAISED: u32 = 0x1d2632;
pub const FIELD: u32 = 0x0d1217;
pub const BORDER: u32 = 0x293341;
pub const SELECTED: u32 = 0x263241;
pub const SELECTED_BORDER: u32 = 0x60758e;
pub const TEXT: u32 = 0xe7edf5;
pub const MUTED: u32 = 0x91a0b4;
pub const ACCENT: u32 = 0xeab875;
pub const GREEN: u32 = 0x74d3ab;
pub const RED: u32 = 0xf08a7e;
pub const WARN_BG: u32 = 0x30281f;
pub const BAD_BG: u32 = 0x3a2224;
pub const GOOD_BG: u32 = 0x1d3029;

use crate::logic::Tone;

pub fn tone(t: Tone) -> u32 {
    match t {
        Tone::Normal => TEXT,
        Tone::Muted => MUTED,
        Tone::Good => GREEN,
        Tone::Warn => ACCENT,
        Tone::Bad => RED,
    }
}
