//! Drawing for the GB350's own screens (the launcher, later the in-game
//! menu): a software canvas over an XRGB8888 buffer, anti-aliased shapes
//! (rendered once into cached coverage masks: there is no FPU, so per-pixel
//! float maths happens only when a mask is first built) and TrueType text
//! (ab_glyph) in the launcher's two typefaces, embedded (both SIL Open Font
//! License, see `fonts/`).

mod canvas;
pub mod display;
pub mod input;
pub mod logview;
pub mod notice;
mod mask;
pub mod style;
pub mod sysstat;
mod text;

pub use canvas::Canvas;
pub use mask::{Mask, MaskBuilder};
pub use text::Face;

use std::collections::HashMap;

/// Long-lived drawing state: fonts, the glyph cache and cached masks.
pub struct Ui {
    fonts: text::Fonts,
    masks: HashMap<String, Mask>,
}

impl Ui {
    pub fn new() -> Ui {
        Ui { fonts: text::Fonts::new(), masks: HashMap::new() }
    }

    /// The mask cached under `key`, built by `build` the first time.
    pub fn mask(&mut self, key: &str, build: impl FnOnce() -> Mask) -> &Mask {
        if !self.masks.contains_key(key) {
            self.masks.insert(key.to_string(), build());
        }
        &self.masks[key]
    }

    /// Width in pixels of `s` set in `face` at `size` px, with `spacing` px
    /// added after every character (CSS letter-spacing).
    pub fn measure(&mut self, face: Face, size: u16, s: &str, spacing: i32) -> i32 {
        self.fonts.measure(face, size, s, spacing)
    }

    /// `s` shortened with an ellipsis to fit `max_w`.
    pub fn ellipsize(&mut self, face: Face, size: u16, s: &str, max_w: i32) -> String {
        self.fonts.ellipsize(face, size, s, max_w)
    }

    /// `s` broken into at most `max_lines` lines of `max_w` (word wrap; the
    /// last line ellipsized).
    pub fn wrap(&mut self, face: Face, size: u16, s: &str, max_w: i32, max_lines: usize) -> Vec<String> {
        self.fonts.wrap(face, size, s, max_w, max_lines)
    }

    /// Baseline that centres a line of `face`/`size` on `cy` (how CSS centres
    /// a line box in a flex row).
    pub fn baseline_center(&mut self, face: Face, size: u16, cy: i32) -> i32 {
        let (asc, desc) = self.fonts.line(face, size);
        cy + (asc + desc) / 2
    }

    /// Baseline of a line whose line box (height `lh`) starts at `top`.
    pub fn baseline_top(&mut self, face: Face, size: u16, top: i32, lh: i32) -> i32 {
        let (asc, desc) = self.fonts.line(face, size);
        top + (lh - (asc - desc)) / 2 + asc
    }
}

impl Default for Ui {
    fn default() -> Self {
        Ui::new()
    }
}

/// Blends `c` over `dst` with coverage `a` (0..=255), all four channels:
/// on a premultiplied ARGB buffer (an overlay plane) this is source-over,
/// on XRGB the top byte is ignored.
#[inline]
pub fn blend(dst: u32, c: u32, a: u32) -> u32 {
    if a >= 255 {
        return c;
    }
    let ia = 255 - a;
    let rb = ((c & 0xff00ff) * a + (dst & 0xff00ff) * ia + 0x800080) >> 8 & 0xff00ff;
    let ag = (((c >> 8) & 0xff00ff) * a + ((dst >> 8) & 0xff00ff) * ia + 0x800080) >> 8 & 0xff00ff;
    rb | ag << 8
}
