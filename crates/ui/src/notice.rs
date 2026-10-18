//! A full-screen notice in the launcher's style: icon, title, explanation,
//! a short list of actions (one selected) and a quiet footer. The agent uses
//! it when the launcher can't run (missing, crashing, no SD card) and while
//! the card is a USB drive.

use crate::style::{self, Badge, ACCENT, BG, DIM, LIST, MUTED, TEXT};
use crate::{Canvas, Face, Mask, MaskBuilder};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Warning,
    Usb,
}

pub struct Notice<'a> {
    pub icon: Icon,
    /// Small caps above the title.
    pub kicker: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub actions: &'a [&'a str],
    pub selected: usize,
    /// Technical detail, shown small at the bottom.
    pub footer: &'a str,
}

fn warning(size: i32) -> Mask {
    let s = size as f32 / 48.0;
    let tri = [(24.0, 4.0), (45.0, 42.0), (3.0, 42.0)];
    let pts: Vec<(f32, f32)> = tri.iter().map(|&(x, y)| (x * s, y * s)).collect();
    MaskBuilder::new(size, size).poly(&pts).build()
}

/// The exclamation mark cut into the warning triangle.
fn bang(size: i32) -> Mask {
    let s = size as f32 / 48.0;
    MaskBuilder::new(size, size).line(24.0 * s, 17.0 * s, 24.0 * s, 29.0 * s, 4.5 * s).disc(24.0 * s, 36.0 * s, 2.6 * s).build()
}

/// The USB trident.
fn usb(size: i32) -> Mask {
    let s = size as f32 / 48.0;
    let arrow: Vec<(f32, f32)> = [(24.0, 2.0), (30.5, 12.0), (17.5, 12.0)].iter().map(|&(x, y)| (x * s, y * s)).collect();
    MaskBuilder::new(size, size)
        .poly(&arrow)
        .line(24.0 * s, 10.0 * s, 24.0 * s, 40.0 * s, 3.5 * s)
        .disc(24.0 * s, 41.5 * s, 5.0 * s)
        .line(24.0 * s, 32.0 * s, 13.0 * s, 24.0 * s, 3.5 * s)
        .line(13.0 * s, 24.0 * s, 13.0 * s, 18.0 * s, 3.5 * s)
        .disc(13.0 * s, 16.0 * s, 4.0 * s)
        .line(24.0 * s, 27.0 * s, 34.0 * s, 20.0 * s, 3.5 * s)
        .line(34.0 * s, 20.0 * s, 34.0 * s, 15.0 * s, 3.5 * s)
        .rrect(30.0 * s, 8.0 * s, 8.0 * s, 8.0 * s, 1.0 * s)
        .build()
}

pub fn draw(c: &mut Canvas, n: &Notice) {
    c.fill(BG);
    let (x, w) = (56, c.w - 112);
    let mut y = 64;
    match n.icon {
        Icon::Warning => {
            c.icon("notice-warn", x, y, ACCENT, || warning(48));
            c.icon("notice-bang", x, y, BG, || bang(48));
        }
        Icon::Usb => c.icon("notice-usb", x, y, ACCENT, || usb(48)),
    }
    y += 48 + 22;
    style::label(c, x, y, n.kicker);
    y += 14;
    for line in c.ui.wrap(Face::DisplayBold, 26, n.title, w, 2) {
        let b = c.ui.baseline_top(Face::DisplayBold, 26, y, 32);
        c.text(Face::DisplayBold, 26, x, b, TEXT, &line);
        y += 32;
    }
    y += 8;
    for line in c.ui.wrap(Face::Sans, 14, n.body, w, 4) {
        c.text_mid(Face::Sans, 14, x, y + 10, MUTED, &line);
        y += 20;
    }
    y += 18;
    for (i, a) in n.actions.iter().enumerate() {
        let on = i == n.selected;
        if on {
            c.round_rect(x - 10, y, 300, 34, 4, ACCENT);
            c.text_mid(Face::SansBold, 15, x, y + 17, BG, a);
        } else {
            c.text_mid(Face::Sans, 15, x, y + 17, LIST, a);
        }
        y += 36;
    }
    if !n.footer.is_empty() {
        let f = c.ui.ellipsize(Face::Sans, 12, n.footer, w);
        c.text_mid(Face::Sans, 12, x, c.h - 36 - 18, DIM, &f);
    }
    let cw = c.w;
    if n.actions.len() > 1 {
        style::hint_bar(c, 0, cw, &[(Badge::Button("A", true), "Select"), (Badge::UpDown, "Choose")], "");
    } else if !n.actions.is_empty() {
        style::hint_bar(c, 0, cw, &[(Badge::Button("A", true), n.actions[0])], "");
    }
}
