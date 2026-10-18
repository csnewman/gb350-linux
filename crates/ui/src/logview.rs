//! A log screen in the launcher's style: header with a status line, then
//! timestamped lines in IBM Plex Mono with a coloured severity marker,
//! newest at the bottom, scrollable, and the button bar.

use crate::style::{self, Badge, ACCENT, BG, DIM, MUTED, RULE, TEXT, WARN};
use crate::{Canvas, Face};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
    /// Typed in from the computer (`gbctl say`, commands).
    Remote,
}

pub struct Entry {
    /// Seconds since boot.
    pub time: f32,
    pub text: String,
    pub level: Level,
}

pub struct LogView<'a> {
    pub title: &'a str,
    pub status: &'a str,
    pub entries: &'a [Entry],
    /// Rows scrolled up from the newest (0 = following the end).
    pub scroll: usize,
    pub hints: &'a [(Badge<'a>, &'a str)],
}

const SIZE: u16 = 12;
const ROW: i32 = 17;
const TOP: i32 = 56;
const TIME_W: i32 = 64;

fn marker(level: Level) -> Option<u32> {
    match level {
        Level::Info => None,
        Level::Ok => Some(0x50C878),
        Level::Warn => Some(ACCENT),
        Level::Error => Some(WARN),
        Level::Remote => Some(0x5AB4E6),
    }
}

/// Breaks `text` into rows of at most `cols` characters, at spaces where
/// possible (monospace, so characters are the measure).
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut rows = vec![];
    let mut cur = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        loop {
            let len = cur.chars().count();
            let need = if len == 0 { word.chars().count() } else { len + 1 + word.chars().count() };
            if need <= cols {
                if len > 0 {
                    cur.push(' ');
                }
                cur.push_str(&word);
                break;
            }
            if len > 0 {
                rows.push(std::mem::take(&mut cur));
                continue;
            }
            // A word longer than a row: hard break.
            let head: String = word.chars().take(cols).collect();
            word = word.chars().skip(cols).collect();
            rows.push(head);
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Rows the log area holds.
pub fn rows(h: i32) -> usize {
    ((h - 36 - 8 - TOP) / ROW) as usize
}

/// Draws the view; returns the largest useful `scroll`.
pub fn draw(c: &mut Canvas, v: &LogView) -> usize {
    c.fill(BG);
    c.text_mid(Face::DisplayBold, 20, 16, 24, TEXT, v.title);
    let status = c.ui.ellipsize(Face::Sans, 12, v.status, c.w - 140);
    c.text_mid_right(Face::Sans, 12, c.w - 16, 24, MUTED, &status);
    c.rect(0, 47, c.w, 1, RULE);

    let text_x = 16 + TIME_W + 12;
    let cols = ((c.w - text_x - 16) / c.ui.measure(Face::Mono, SIZE, "0", 0).max(1)) as usize;
    // Lay out from the newest entry up until the screen (and scroll) is full.
    let visible = rows(c.h);
    let mut laid: Vec<(Option<&Entry>, String)> = vec![];
    for e in v.entries.iter().rev() {
        let mut rs: Vec<(Option<&Entry>, String)> = wrap(&e.text, cols).into_iter().map(|r| (None, r)).collect();
        rs[0].0 = Some(e);
        laid.splice(0..0, rs);
        if laid.len() >= visible + v.scroll {
            break;
        }
    }
    let max_scroll = laid.len().saturating_sub(visible);
    let scroll = v.scroll.min(max_scroll);
    let end = laid.len() - scroll;
    let start = end.saturating_sub(visible);
    let mut y = TOP;
    let mut bar: Option<(u32, i32)> = None;
    for (entry, row) in &laid[start..end] {
        if let Some(e) = entry {
            c.text_mid_right(Face::Mono, SIZE, 16 + TIME_W, y + ROW / 2, DIM, &format!("{:.2}", e.time));
            bar = marker(e.level).map(|m| (m, y));
        }
        if let Some((m, _)) = bar {
            c.rect(text_x - 8, y + 2, 3, ROW - 4, m);
        }
        let col = if matches!(bar, Some((m, _)) if m == WARN) { 0xFFB3A8 } else { TEXT };
        c.text_mid(Face::Mono, SIZE, text_x, y + ROW / 2, col, row);
        y += ROW;
    }
    if scroll > 0 {
        let s = format!("{scroll} more below");
        let w = c.ui.measure(Face::Sans, 11, &s, 0) + 16;
        c.round_rect(c.w - 16 - w, c.h - 36 - 30, w, 22, 11, 0x26292F);
        c.text_mid_center(Face::Sans, 11, c.w - 16 - w / 2, c.h - 36 - 19, MUTED, &s);
    }
    let cw = c.w;
    style::hint_bar(c, 0, cw, v.hints, "");
    max_scroll
}
