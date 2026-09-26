//! Android `KeyEvent` codes and meta flags the panel sends (the values are
//! Android's `AKEYCODE_*` / `AMETA_*`; they never change).

pub const HOME: u32 = 3;
pub const BACK: u32 = 4;
pub const DPAD_UP: u32 = 19;
pub const DPAD_DOWN: u32 = 20;
pub const DPAD_LEFT: u32 = 21;
pub const DPAD_RIGHT: u32 = 22;
pub const VOLUME_UP: u32 = 24;
pub const VOLUME_DOWN: u32 = 25;
pub const POWER: u32 = 26;
pub const TAB: u32 = 61;
pub const ENTER: u32 = 66;
pub const DEL: u32 = 67;
pub const PAGE_UP: u32 = 92;
pub const PAGE_DOWN: u32 = 93;
pub const ESCAPE: u32 = 111;
pub const FORWARD_DEL: u32 = 112;
pub const MOVE_HOME: u32 = 122;
pub const MOVE_END: u32 = 123;
pub const APP_SWITCH: u32 = 187;

pub const META_SHIFT_ON: u32 = 0x01;
pub const META_ALT_ON: u32 = 0x02;
pub const META_CTRL_ON: u32 = 0x1000;
pub const META_META_ON: u32 = 0x10000;
