//! The look of the GB350 screens (launcher design A, "Shelf"): colours and the
//! small widgets shared by the launcher and the in-game menu.

use crate::{Canvas, Face, Mask, MaskBuilder};

pub const BG: u32 = 0x121316;
pub const TEXT: u32 = 0xEDEDED;
pub const ACCENT: u32 = 0xFFC93C;
/// The accent, darkened: tracks drawn on an accent background.
pub const ACCENT_DARK: u32 = 0xA8862A;
pub const MUTED: u32 = 0xA3A8B0;
pub const DIM: u32 = 0x8A9099;
pub const LIST: u32 = 0xC9CDD3;
pub const PANEL: u32 = 0x1C1E23;
pub const PANEL_EDGE: u32 = 0x2C3038;
pub const RULE: u32 = 0x262930;
pub const SEL_ROW: u32 = 0x26292F;
pub const HINT_BG: u32 = 0x0D0E10;
pub const STRIP: u32 = 0x6F7680;
pub const TRACK: u32 = 0x3A3E46;
pub const WARN: u32 = 0xFF6B5A;

// ---- icons (24-unit artboards from the mockups, scaled) ----

pub fn star(size: i32) -> Mask {
    let s = size as f32 / 24.0;
    let p = [(12.0, 3.0), (14.7, 8.6), (20.8, 9.5), (16.4, 13.8), (17.4, 19.9), (12.0, 17.0), (6.6, 19.9), (7.6, 13.8), (3.2, 9.5), (9.3, 8.6)];
    let pts: Vec<(f32, f32)> = p.iter().map(|&(x, y)| (x * s, y * s)).collect();
    MaskBuilder::new(size, size).poly(&pts).build()
}

/// A 7x9 arrowhead pointing left or right.
pub fn triangle(left: bool) -> Mask {
    let pts = if left { [(7.0, 0.5), (7.0, 8.5), (0.5, 4.5)] } else { [(0.0, 0.5), (0.0, 8.5), (6.5, 4.5)] };
    MaskBuilder::new(7, 9).poly(&pts).build()
}

/// A loudspeaker, `size` px.
pub fn speaker(size: i32) -> Mask {
    let s = size as f32 / 24.0;
    let body = [(4.0, 9.0), (8.0, 9.0), (13.0, 5.0), (13.0, 19.0), (8.0, 15.0), (4.0, 15.0)];
    let pts: Vec<(f32, f32)> = body.iter().map(|&(x, y)| (x * s, y * s)).collect();
    MaskBuilder::new(size, size).poly(&pts).line(16.5 * s, 9.0 * s, 17.5 * s, 12.0 * s, 2.0 * s).line(17.5 * s, 12.0 * s, 16.5 * s, 15.0 * s, 2.0 * s).build()
}

// ---- widgets ----

pub enum Badge<'a> {
    /// A round button; filled for the main action.
    Button(&'a str, bool),
    Pill(&'a str),
    /// The left/right arrows pill.
    Arrows,
    /// The up/down arrows pill.
    UpDown,
}

/// Draws a button badge with its left edge at `x`, centred on `cy`;
/// returns its width.
pub fn badge(c: &mut Canvas, b: &Badge, x: i32, cy: i32) -> i32 {
    match b {
        Badge::Button(label, filled) => {
            if *filled {
                c.disc(x, cy - 10, 20, ACCENT);
                c.text_mid_center(Face::SansBold, 12, x + 10, cy, BG, label);
            } else {
                c.ring(x, cy - 10, 20, 1.5, MUTED);
                c.text_mid_center(Face::SansBold, 12, x + 10, cy, LIST, label);
            }
            20
        }
        Badge::Pill(label) => {
            let w = c.ui.measure(Face::SansBold, 11, label, 0) + 15;
            c.round_rect_stroke(x, cy - 10, w, 20, 10.0, 1.5, MUTED);
            c.text_mid_center(Face::SansBold, 11, x + w / 2, cy, LIST, label);
            w
        }
        Badge::Arrows => {
            let w = 36;
            c.round_rect_stroke(x, cy - 10, w, 20, 10.0, 1.5, MUTED);
            c.icon("tri-l", x + 9, cy - 4, LIST, || triangle(true));
            c.icon("tri-r", x + 20, cy - 4, LIST, || triangle(false));
            w
        }
        Badge::UpDown => {
            let w = 36;
            c.round_rect_stroke(x, cy - 10, w, 20, 10.0, 1.5, MUTED);
            c.icon("tri-u", x + 8, cy - 3, LIST, || MaskBuilder::new(9, 7).poly(&[(0.5, 6.5), (8.5, 6.5), (4.5, 0.0)]).build());
            c.icon("tri-d", x + 19, cy - 3, LIST, || MaskBuilder::new(9, 7).poly(&[(0.5, 0.5), (8.5, 0.5), (4.5, 7.0)]).build());
            w
        }
    }
}

/// The button hints bar: 36 px tall across `x0..x1` at the bottom.
pub fn hint_bar(c: &mut Canvas, x0: i32, x1: i32, hints: &[(Badge, &str)], right: &str) {
    let (y, h) = (c.h - 36, 36);
    c.rect(x0, y, x1 - x0, h, HINT_BG);
    let cy = y + h / 2;
    let mut x = x0 + 16;
    for (b, label) in hints {
        x += badge(c, b, x, cy) + 6;
        x += c.text_mid(Face::Sans, 13, x, cy, LIST, label) + 18;
    }
    if !right.is_empty() {
        c.text_mid_right(Face::Sans, 13, x1 - 16, cy, DIM, right);
    }
}

/// Section label: small caps with letter spacing.
pub fn label(c: &mut Canvas, x: i32, cy: i32, s: &str) {
    let b = c.ui.baseline_center(Face::Sans, 11, cy);
    c.text_ls(Face::Sans, 11, x, b, DIM, s, 1);
}

/// A level slider across `x0..x1` centred on `cy`, with a knob when
/// `knob` is set.
pub fn slider(c: &mut Canvas, x0: i32, x1: i32, cy: i32, pct: u32, track: u32, fill: u32, knob: bool) {
    let w = (x1 - x0) * pct.min(100) as i32 / 100;
    c.round_rect(x0, cy - 3, x1 - x0, 6, 3, track);
    c.round_rect(x0, cy - 3, w.max(6), 6, 3, fill);
    if knob {
        c.disc(x0 + w - 8, cy - 8, 16, fill);
    }
}

/// "◀ value ▶", right-aligned to `right`.
pub fn choice(c: &mut Canvas, right: i32, cy: i32, value: &str, text: u32, arrows: u32) {
    c.icon("tri-r", right - 7, cy - 4, arrows, || triangle(false));
    let vx = right - 7 - 8;
    let vw = c.text_mid_right(Face::Sans, 14, vx, cy, text, value);
    c.icon("tri-l", vx - vw - 8 - 7, cy - 4, arrows, || triangle(true));
}

/// 13310 -> "13,310".
pub fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
