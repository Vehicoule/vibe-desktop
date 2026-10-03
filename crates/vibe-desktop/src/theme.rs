//! Mistral-vibe palette: warm ivory surfaces, sunset accents, ink text.

use gpui::{rgb, Hsla, Styled as _};

pub const IVORY: u32 = 0xfdf7ef;
pub const IVORY_DEEP: u32 = 0xf6edde;
pub const PAPER: u32 = 0xfffbf5;
pub const CARD: u32 = 0xfff5ea;
pub const EDGE: u32 = 0xe8dcc9;
pub const INK: u32 = 0x2b2118;
pub const INK_SOFT: u32 = 0x6b5d4f;
pub const INK_FAINT: u32 = 0x9c8f7f;
pub const SUNSET: u32 = 0xff5a1f; // core orange
pub const SUNSET_DEEP: u32 = 0xe03c00; // red edge
pub const SUNSET_WASH: u32 = 0xffe3d1;
pub const AMBER: u32 = 0xd97e00;
pub const GREEN: u32 = 0x3d7a4f;
pub const RED: u32 = 0xc53b2c;
pub const BLUEGREY: u32 = 0x5b6b7a;

pub fn c(v: u32) -> Hsla {
    rgb(v).into()
}

/// Pixel-block square used for the Mistral mosaic motif.
pub fn pixel(size: f32, color: u32) -> gpui::Div {
    gpui::div().w(gpui::px(size)).h(gpui::px(size)).bg(c(color))
}
