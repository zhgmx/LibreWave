//! Shared color, layout, and type roles for the desktop client.

// Cool near-black surfaces that sit under the steel tones of the device art.
pub(super) const BACKGROUND: u32 = 0x0007_080A;
pub(super) const PANEL: u32 = 0x0010_1216;
// A group of rows inside a dialog.
pub(super) const GROUP: u32 = 0x0016_1920;
pub(super) const RAISED: u32 = 0x001B_1E26;
pub(super) const RAISED_HOVER: u32 = 0x0025_2933;
// Hairlines inside a surface and the unlit part of a level track.
pub(super) const LINE: u32 = 0x0023_2730;
pub(super) const INK: u32 = 0x00F2_F4F7;
pub(super) const MUTED: u32 = 0x0097_9FAD;
pub(super) const FAINT: u32 = 0x0060_6875;

// One color for each meaning. Monitor and Stream keep their color everywhere.
pub(super) const MONITOR: u32 = 0x006B_EBB5;
pub(super) const STREAM: u32 = 0x007A_B6FF;
pub(super) const DANGER: u32 = 0x00FF_4D5E;
pub(super) const WARNING: u32 = 0x00FF_B65C;
pub(super) const FOCUS: u32 = 0x0070_B7FF;

/// Commands, paths, and command output.
pub(super) const MONO: &str = "Geist Mono";

pub(super) const SPACE_XS: f32 = 4.;
pub(super) const SPACE_SM: f32 = 8.;
pub(super) const SPACE_MD: f32 = 12.;
pub(super) const SPACE_LG: f32 = 16.;
pub(super) const SPACE_XL: f32 = 24.;
pub(super) const SPACE_2XL: f32 = 32.;

pub(super) const LABEL: f32 = 11.;
pub(super) const CAPTION: f32 = 12.;
pub(super) const BODY: f32 = 14.;
pub(super) const HEADING: f32 = 16.;
pub(super) const TITLE: f32 = 20.;
pub(super) const DISPLAY: f32 = 32.;
pub(super) const LABEL_LINE: f32 = 16.;
pub(super) const CAPTION_LINE: f32 = 16.;
pub(super) const BODY_LINE: f32 = 20.;
pub(super) const HEADING_LINE: f32 = 24.;
pub(super) const TITLE_LINE: f32 = 28.;
pub(super) const DISPLAY_LINE: f32 = 40.;

pub(super) const CONTROL: f32 = 32.;
pub(super) const ICON: f32 = 20.;
pub(super) const ICON_SMALL: f32 = 16.;
pub(super) const RADIUS: f32 = 8.;
pub(super) const RADIUS_CARD: f32 = 14.;

/// Adds an alpha byte to an opaque theme color.
pub(super) const fn alpha(color: u32, alpha: u8) -> u32 {
    (color << 8) | alpha as u32
}
