//! Drawing: design A ("Shelf") at 640x480 - dark, yellow accent, Chakra
//! Petch for display text and IBM Plex Sans for the rest.

use crate::app::{self, App, KeyCap, Row, Screen, KEY_COLS, LIST_ROWS, RESULT_ROWS, SECTIONS};
use crate::library::{size_text, Tab, SYSTEMS};
use crate::settings::{Border, Filter, Scaling};
use gb350_ui::style::{self, group, label, star, Badge, ACCENT, BG, DIM, LIST, MUTED, PANEL, PANEL_EDGE, RULE, SEL_ROW, STRIP, TEXT, TRACK, WARN};
use gb350_ui::{Canvas, Face, Mask, MaskBuilder};

fn hint_bar(c: &mut Canvas, hints: &[(Badge, &str)], right: &str) {
    let w = c.w;
    style::hint_bar(c, 0, w, hints, right);
}


/// Preview image box (inside its 1 px border).
pub const PREVIEW_W: u32 = 258;
pub const PREVIEW_H: u32 = 166;

// ---- icons (24-unit artboards from the mockups, scaled) ----

fn magnifier(size: i32) -> Mask {
    let s = size as f32 / 24.0;
    MaskBuilder::new(size, size).ring(10.5 * s, 10.5 * s, 7.5 * s, 2.0 * s).line(15.5 * s, 15.5 * s, 21.0 * s, 21.0 * s, 2.0 * s).build()
}

fn gear(size: i32) -> Mask {
    let s = size as f32 / 24.0;
    let mut b = MaskBuilder::new(size, size).ring(12.0 * s, 12.0 * s, 4.2 * s, 2.0 * s);
    let spokes = [
        (12.0, 2.5, 12.0, 5.5),
        (12.0, 18.5, 12.0, 21.5),
        (2.5, 12.0, 5.5, 12.0),
        (18.5, 12.0, 21.5, 12.0),
        (5.3, 5.3, 7.4, 7.4),
        (16.6, 16.6, 18.7, 18.7),
        (5.3, 18.7, 7.4, 16.6),
        (16.6, 7.4, 18.7, 5.3),
    ];
    for (x0, y0, x1, y1) in spokes {
        b = b.line(x0 * s, y0 * s, x1 * s, y1 * s, 2.0 * s);
    }
    b.build()
}

// ---- shared pieces ----

fn status_bar(c: &mut Canvas, app: &App) {
    let n = app.lib.total();
    let left = format!("{} game{}", group(n), if n == 1 { "" } else { "s" });
    c.text_mid(Face::Sans, 13, 16, 14, MUTED, &left);
    let mut right = c.w - 16;
    if let Some((cap, status)) = &app.battery {
        right -= 26;
        let (x, y) = (right, 7);
        c.icon("battery", x, y, MUTED, || MaskBuilder::new(26, 14).rrect_stroke(1.0, 1.0, 21.0, 12.0, 2.0, 1.5).build());
        c.rect(x + 23, y + 4, 2, 6, MUTED);
        let fill = (16 * (*cap).min(100) as i32 / 100).max(1);
        c.rect(x + 3, y + 3, fill, 8, if *cap <= 15 { WARN } else { MUTED });
        let label = if status == "Charging" { format!("{cap}% charging") } else { format!("{cap}%") };
        right -= 8;
        c.text_mid_right(Face::Sans, 13, right, 14, MUTED, &label);
        right -= c.ui.measure(Face::Sans, 13, &label, 0) + 12;
    }
    if app.headphones {
        c.text_mid_right(Face::Sans, 13, right, 14, MUTED, "Headphones");
    }
}

fn initials(name: &str) -> String {
    name.split(|ch: char| ch.is_whitespace() || ch == '-' || ch == ':')
        .filter_map(|w| w.chars().find(|ch| ch.is_alphanumeric()))
        .filter(|ch| ch.is_ascii())
        .take(3)
        .collect::<String>()
        .to_uppercase()
}

// ---- screens ----

pub fn draw(c: &mut Canvas, app: &mut App) {
    c.fill(BG);
    status_bar(c, app);
    match app.screen {
        Screen::Library => library(c, app),
        Screen::Search => search(c, app),
        Screen::Settings => settings(c, app),
        Screen::Storage => storage(c, app),
    }
    if let Some(t) = &app.toast {
        toast(c, &t.text, t.bar);
    }
    if let Some(title) = &app.loading {
        overlay(c, "Starting", title, "In the game, hold SELECT + START for the menu");
    } else if app.powering_off {
        overlay(c, "Goodbye", "Powering off", "");
    }
}

fn tab_label(t: Tab) -> &'static str {
    match t {
        Tab::Recent => "RECENT",
        Tab::Favourites => "",
        Tab::System(s) => SYSTEMS[s].short,
    }
}

fn library(c: &mut Canvas, app: &mut App) {
    // System tabs.
    c.rect(0, 83, c.w, 1, RULE);
    let lw = c.ui.measure(Face::Display, 13, "L", 0) + 12;
    c.round_rect(12, 46, lw, 20, 4, MUTED);
    c.text_mid_center(Face::Display, 13, 12 + lw / 2, 56, BG, "L");
    let rw = c.ui.measure(Face::Display, 13, "R", 0) + 12;
    c.round_rect(c.w - 12 - rw, 46, rw, 20, 4, MUTED);
    c.text_mid_center(Face::Display, 13, c.w - 12 - rw / 2, 56, BG, "R");
    let (x0, x1) = (12 + lw + 6, c.w - 12 - rw - 6);
    let counts: Vec<usize> = app.tabs.iter().map(|&t| app.lib.list(t).len()).collect();
    let widths: Vec<i32> = app
        .tabs
        .iter()
        .zip(&counts)
        .map(|(&t, &n)| {
            let name = if t == Tab::Favourites { 16 } else { c.ui.measure(Face::DisplayBold, 17, tab_label(t), 0) };
            name.max(c.ui.measure(Face::Sans, 11, &group(n), 0)) + 16
        })
        .collect();
    // Scroll so the current tab is visible, then fill the row.
    let avail = x1 - x0;
    let fits = |a: usize, b: usize| widths[a..=b].iter().sum::<i32>() + 4 * (b - a) as i32 <= avail;
    if app.tab < app.tab_scroll {
        app.tab_scroll = app.tab;
    }
    while app.tab_scroll < app.tab && !fits(app.tab_scroll, app.tab) {
        app.tab_scroll += 1;
    }
    let first = app.tab_scroll.min(app.tabs.len().saturating_sub(1));
    let mut last = first;
    while last + 1 < app.tabs.len() && fits(first, last + 1) {
        last += 1;
    }
    if !app.tabs.is_empty() {
        let used: i32 = widths[first..=last].iter().sum();
        let gaps = (last - first) as i32;
        // Spread a full row out; centre a short one with even gaps.
        let gap = if gaps > 0 { ((avail - used) / gaps).min(28) } else { 0 };
        let mut x = x0 + (avail - used - gap * gaps) / 2;
        for i in first..=last {
            let (t, w, on) = (app.tabs[i], widths[i], i == app.tab);
            let cx = x + w / 2;
            let col = if on { ACCENT } else { TEXT };
            if t == Tab::Favourites {
                c.icon("star16", cx - 8, 40, col, || star(16));
            } else {
                let face = if on { Face::DisplayBold } else { Face::Display };
                c.text_mid_center(face, 17, cx, 48, col, tab_label(t));
            }
            c.text_mid_center(Face::Sans, 11, cx, 66, if on { ACCENT } else { MUTED }, &group(counts[i]));
            if on {
                c.rect(x, 77, w, 3, ACCENT);
            }
            x += w + gap;
        }
    }

    if app.list.is_empty() {
        c.text_mid_center(Face::DisplayBold, 21, c.w / 2, 220, TEXT, "No games found");
        c.text_mid_center(Face::Sans, 13, c.w / 2, 250, MUTED, "Put games in a folder per system on the SD card,");
        c.text_mid_center(Face::Sans, 13, c.w / 2, 270, MUTED, "e.g. PS/, SFC/ or GBA/, then rescan in Settings.");
        hint_bar(c, &[(Badge::Button("X", false), "Search"), (Badge::Pill("START"), "Settings")], "");
        return;
    }

    // Game list.
    let tab = app.current_tab();
    let mixed = matches!(tab, Some(Tab::Recent | Tab::Favourites));
    let (lx, ly, lw) = (12, 94, 318);
    for r in 0..LIST_ROWS {
        let i = app.top + r;
        let Some(&g) = app.list.get(i) else { break };
        let y = ly + r as i32 * 34;
        let on = i == app.sel;
        if on {
            c.round_rect(lx, y, lw, 32, 4, ACCENT);
        }
        let mut right = lx + lw - 10;
        if app.lib.is_favourite(g) {
            if on {
                c.icon("star14", right - 14, y + 9, BG, || star(14));
                right -= 20;
            } else {
                c.icon("star12", right - 12, y + 10, DIM, || star(12));
                right -= 18;
            }
        }
        if mixed {
            let tag = SYSTEMS[app.lib.games[g as usize].sys].short;
            right -= c.text_mid_right(Face::SansBold, 10, right, y + 16, if on { BG } else { MUTED }, tag) + 8;
        }
        let (face, col) = if on { (Face::SansBold, BG) } else { (Face::Sans, LIST) };
        let name = c.ui.ellipsize(face, 15, &app.lib.games[g as usize].name, right - lx - 10);
        c.text_mid(face, 15, lx + 10, y + 16, col, &name);
    }

    // A-Z strip (sorted lists only).
    if tab != Some(Tab::Recent) {
        let mut letters: Vec<char> = vec![];
        for &g in &app.list {
            let l = app.lib.games[g as usize].letter;
            if letters.last() != Some(&l) {
                letters.push(l);
            }
        }
        let cur = app.lib.games[app.list[app.sel] as usize].letter;
        let (sx, sy, sh) = (342 + 7, 96, 338);
        let n = letters.len() as i32;
        for (i, &l) in letters.iter().enumerate() {
            let cy = sy + (i as i32 * 2 + 1) * sh / (2 * n);
            let s = l.to_string();
            if l == cur {
                let w = c.ui.measure(Face::SansBold, 9, &s, 0) + 6;
                c.round_rect(sx - w / 2, cy - 7, w, 14, 3, ACCENT);
                c.text_mid_center(Face::SansBold, 9, sx, cy, BG, &s);
            } else {
                c.text_mid_center(Face::SansBold, 9, sx, cy, STRIP, &s);
            }
        }
    }

    if let Some(g) = app.selected() {
        preview(c, app, g, 368, 94, 260);
    }

    hint_bar(
        c,
        &[(Badge::Button("A", true), "Play"), (Badge::Button("X", false), "Search"), (Badge::Button("Y", false), "Favourite"), (Badge::Pill("START"), "Settings")],
        "L2 / R2 letter",
    );
}

fn preview(c: &mut Canvas, app: &mut App, g: u32, x: i32, y: i32, w: i32) {
    let h = PREVIEW_H as i32 + 2;
    c.round_rect(x, y, w, h, 6, PANEL_EDGE);
    c.round_rect(x + 1, y + 1, w - 2, h - 2, 5, PANEL);
    let game_name = app.lib.games[g as usize].name.clone();
    match app.previews.get(&app.lib.image_path(g)) {
        Some(Some(img)) => {
            let (ix, iy) = (x + 1 + (w - 2 - img.w) / 2, y + 1 + (h - 2 - img.h) / 2);
            c.image(ix, iy, img.w, img.h, &img.px);
        }
        Some(None) => {
            c.text_mid_center(Face::DisplayBold, 44, x + w / 2, y + h / 2 - 8, ACCENT, &initials(&game_name));
            c.text_mid_center(Face::Sans, 11, x + w / 2, y + h / 2 + 28, DIM, "No screenshot");
        }
        None => {}
    }
    // Title (up to two lines), then details.
    let mut ty = y + h + 10;
    for line in c.ui.wrap(Face::DisplayBold, 21, &game_name, w, 2) {
        let b = c.ui.baseline_top(Face::DisplayBold, 21, ty, 23);
        c.text(Face::DisplayBold, 21, x, b, TEXT, &line);
        ty += 23;
    }
    ty += 10;
    let sys = &SYSTEMS[app.lib.games[g as usize].sys];
    let size = std::fs::metadata(app.lib.full_path(g)).map(|m| size_text(m.len())).unwrap_or_default();
    let mut lines: Vec<(String, u32)> = vec![];
    if sys.core.is_none() {
        lines.push((format!("No {} emulator yet", sys.short), ACCENT));
    }
    lines.push((c.ui.ellipsize(Face::Sans, 13, &format!("{} · {} · {size}", sys.name, app.lib.format(g)), w), MUTED));
    if app.lib.is_favourite(g) {
        lines.push(("★ Favourite".into(), MUTED));
    }
    if let Some(i) = app.lib.recent.iter().position(|p| *p == app.lib.games[g as usize].path) {
        lines.push((if i == 0 { "Played last".into() } else { "Played recently".into() }, MUTED));
    }
    for (text, col) in lines {
        if let Some(rest) = text.strip_prefix("★ ") {
            c.icon("star12", x, ty + 3, ACCENT, || star(12));
            c.text_mid(Face::Sans, 13, x + 16, ty + 9, col, rest);
        } else {
            c.text_mid(Face::Sans, 13, x, ty + 9, col, &text);
        }
        ty += 21;
    }
}

fn search(c: &mut Canvas, app: &mut App) {
    let s = &app.search;
    // Query box.
    c.round_rect(12, 32, c.w - 24, 48, 6, ACCENT);
    c.round_rect(14, 34, c.w - 28, 44, 4, PANEL);
    c.icon("magnifier20", 28, 46, ACCENT, || magnifier(20));
    let mut tx = 58;
    if s.query.is_empty() {
        let hint = format!("Search {} games", group(app.lib.total()));
        c.text_mid(Face::Display, 22, tx + 4, 56, STRIP, &hint);
    } else {
        tx += c.text_mid(Face::Display, 22, tx, 56, TEXT, &s.query) + 2;
    }
    if !s.in_results {
        c.rect(tx, 44, 2, 24, ACCENT);
    }
    let scope = s.filter.map(|f| SYSTEMS[f].name).unwrap_or("all systems");
    c.text_mid_right(Face::Sans, 12, c.w - 28, 56, MUTED, scope);

    // Keyboard.
    let (kx, ky, kw) = (12, 92, 286);
    let cw = (kw - (KEY_COLS as i32 - 1) * 4) as f32 / KEY_COLS as f32;
    let col_x = |col: usize| kx + (col as f32 * (cw + 4.0)).round() as i32;
    for (i, (col, row, span, cap)) in app::keyboard().into_iter().enumerate() {
        let x = col_x(col);
        let w = col_x(col + span) - 4 - x;
        let y = ky + row as i32 * 42;
        let on = i == s.key;
        let (bg, fg) = if on && !s.in_results { (ACCENT, BG) } else { (PANEL, if on { ACCENT } else { TEXT }) };
        c.round_rect(x, y, w, 38, 4, bg);
        match cap {
            KeyCap::Char(ch) => c.text_mid_center(Face::Display, 17, x + w / 2, y + 19, fg, &ch.to_string()),
            KeyCap::Space => c.text_mid_center(Face::Sans, 13, x + w / 2, y + 19, if on && !s.in_results { BG } else { MUTED }, "space"),
            KeyCap::Delete => c.text_mid_center(Face::Sans, 13, x + w / 2, y + 19, if on && !s.in_results { BG } else { MUTED }, "delete"),
        };
    }

    // Results.
    let (rx, ry, rw) = (310, 92, c.w - 12 - 310);
    if s.query.is_empty() {
        label(c, rx + 8, ry + 7, "SEARCH");
        c.text_mid(Face::Sans, 13, rx + 8, ry + 34, MUTED, "Type part of a name.");
        c.text_mid(Face::Sans, 13, rx + 8, ry + 54, MUTED, "L / R limits it to one system.");
    } else if s.results.is_empty() {
        label(c, rx + 8, ry + 7, "NO MATCHES");
    } else {
        label(c, rx + 8, ry + 7, if s.in_results { "RESULTS" } else { "BEST MATCHES" });
        let n = s.results.len();
        let count = if n >= 300 { "300+".to_string() } else { group(n) };
        c.text_mid_right(Face::Sans, 11, rx + rw - 8, ry + 7, DIM, &count);
        let q = s.query.trim().to_lowercase();
        for r in 0..RESULT_ROWS {
            let i = s.top + r;
            let Some(&g) = s.results.get(i) else { break };
            let y = ry + 18 + r as i32 * 32;
            let on = i == s.sel;
            if on {
                c.round_rect(rx, y, rw, 30, 4, SEL_ROW);
                if s.in_results {
                    c.rect(rx, y + 4, 3, 22, ACCENT);
                }
            }
            let game = &app.lib.games[g as usize];
            c.text_mid(Face::SansBold, 10, rx + 8, y + 15, if on { ACCENT } else { MUTED }, SYSTEMS[game.sys].short);
            let nx = rx + 50;
            let max = rx + rw - 8 - nx;
            let shown = c.ui.ellipsize(Face::Sans, 14, &game.name, max);
            // Bold the matched part, when it is on screen as typed.
            let at = (game.name.len() == game.key.len()).then(|| game.key.find(&q)).flatten();
            let visible = shown.trim_end_matches('…').len();
            match at {
                Some(a) if a + q.len() <= visible && shown.is_char_boundary(a) && shown.is_char_boundary(a + q.len()) => {
                    let mut x = nx;
                    x += c.text_mid(Face::Sans, 14, x, y + 15, TEXT, &shown[..a]);
                    x += c.text_mid(Face::SansBold, 14, x, y + 15, TEXT, &shown[a..a + q.len()]);
                    c.text_mid(Face::Sans, 14, x, y + 15, TEXT, &shown[a + q.len()..]);
                }
                _ => {
                    c.text_mid(Face::Sans, 14, nx, y + 15, TEXT, &shown);
                }
            }
        }
    }

    if s.in_results {
        hint_bar(c, &[(Badge::Button("A", true), "Play"), (Badge::Button("B", false), "Keyboard"), (Badge::Button("Y", false), "Favourite")], "L / R system");
    } else {
        let b = if s.query.is_empty() { "Back" } else { "Delete" };
        hint_bar(
            c,
            &[(Badge::Button("A", true), "Type"), (Badge::Button("B", false), b), (Badge::Button("Y", false), "Space"), (Badge::Pill("START"), "Results")],
            "L / R system",
        );
    }
}

fn settings(c: &mut Canvas, app: &mut App) {
    c.icon("gear18", 16, 41, ACCENT, || gear(18));
    c.text_mid(Face::DisplayBold, 20, 44, 50, TEXT, "Settings");
    c.rect(0, 71, c.w, 1, RULE);

    for (i, name) in SECTIONS.iter().enumerate() {
        let y = 82 + i as i32 * 34;
        if i == app.set.section {
            c.round_rect(12, y, 168, 32, 4, ACCENT);
            c.text_mid(Face::SansBold, 15, 22, y + 16, BG, name);
        } else {
            c.text_mid(Face::Sans, 15, 22, y + 16, LIST, name);
        }
    }

    let (px, pw) = (192, c.w - 12 - 192);
    let rs = app::rows(app.set.section);
    // Layout (top, height) of every row in list coordinates, then scroll so
    // the selected row - with its heading right above it - is in the area
    // between the top rule and the help text.
    let (top, bottom) = (82, 380);
    let mut layout = Vec::with_capacity(rs.len());
    let mut y = 0;
    for (i, &row) in rs.iter().enumerate() {
        if let Row::Header(_) = row {
            if i > 0 {
                y += 8;
            }
            layout.push((y, 20));
            y += 20;
        } else {
            let h = if i == app.set.row { 40 } else { 36 };
            layout.push((y, h));
            y += h + 2;
        }
    }
    let total = y;
    let sel = app.set.row;
    let first = if sel > 0 && matches!(rs[sel - 1], Row::Header(_)) { layout[sel - 1].0 } else { layout[sel].0 };
    let last = layout[sel].0 + layout[sel].1;
    let area = bottom - top;
    let scroll = &mut app.set.scroll;
    if first < *scroll {
        *scroll = first;
    } else if last > *scroll + area {
        *scroll = last - area;
    }
    *scroll = (*scroll).clamp(0, (total - area).max(0));
    let scroll = *scroll;
    for (i, &row) in rs.iter().enumerate() {
        let (ly, h) = layout[i];
        let y = top + ly - scroll;
        // Only rows wholly inside the area.
        if y < top || y + h > bottom + 2 {
            continue;
        }
        if let Row::Header(name) = row {
            label(c, px + 10, y + 9, name);
            continue;
        }
        let on = i == app.set.row;
        if on {
            c.round_rect(px, y, pw, h, 4, ACCENT);
            c.round_rect(px + 3, y, pw - 3, h, 4, SEL_ROW);
            c.rect(px + 3, y, 4, h, SEL_ROW);
        }
        settings_row(c, app, row, px, y, pw, h, on);
    }
    // More above / below.
    let chevron = |up: bool| {
        let pts = if up { [(0.5, 6.5), (10.5, 6.5), (5.5, 0.5)] } else { [(0.5, 0.5), (10.5, 0.5), (5.5, 6.5)] };
        gb350_ui::MaskBuilder::new(11, 7).poly(&pts).build()
    };
    if scroll > 0 {
        c.icon("chevron-up", px + pw - 20, top - 9, MUTED, || chevron(true));
    }
    if scroll < total - area {
        c.icon("chevron-down", px + pw - 20, bottom + 3, MUTED, || chevron(false));
    }
    // Help for the selected row, at the bottom of the panel.
    let help = rs[app.set.row].help();
    if !help.is_empty() {
        let lines = c.ui.wrap(Face::Sans, 12, help, pw - 20, 3);
        let mut ly = 436 - lines.len() as i32 * 17;
        for l in lines {
            c.text_mid(Face::Sans, 12, px + 10, ly + 8, DIM, &l);
            ly += 17;
        }
    }

    let row = rs[app.set.row];
    if row.action() {
        hint_bar(c, &[(Badge::Button("A", true), "Select"), (Badge::Button("B", false), "Back"), (Badge::Pill("L / R"), "Section")], "");
    } else {
        hint_bar(c, &[(Badge::Arrows, "Change"), (Badge::Button("B", false), "Back"), (Badge::Pill("L / R"), "Section")], "Saved as you change");
    }
}

#[allow(clippy::too_many_arguments)]
fn settings_row(c: &mut Canvas, app: &App, row: Row, x: i32, y: i32, w: i32, h: i32, on: bool) {
    let cy = y + h / 2;
    let right = x + w - 10;
    let s = &app.settings;
    let label_text = match row {
        Row::Brightness => "Brightness",
        Row::Scaling => "Game scaling",
        Row::Filter => "Game filter",
        Row::Volume => "Volume",
        Row::Headphones => "Headphones",
        Row::Battery => "Battery",
        Row::PowerOff => "Power off",
        Row::Games => "Games",
        Row::Rescan => "Rescan the SD card",
        Row::Card => "SD card",
        Row::Linux => "Linux",
        Row::Syscore => "Second-core firmware",
        Row::System => "System",
        Row::Cpu => "CPU",
        Row::Memory => "Memory",
        Row::SecondCore => "Second core",
        Row::Uptime => "Uptime",
        Row::Perf => "Stats overlay",
        Row::Vertical => "Vertical games",
        Row::Ps1Cores => "PS1 drawing",
        Row::Colour => "Colour",
        Row::Contrast => "Contrast",
        Row::Sharpness => "Sharpness",
        Row::Border => "Border",
        Row::ScreenOff => "Screen off",
        Row::ExitConsole => "Exit to log",
        Row::Storage => "Connect to computer",
        Row::UsbControl => "USB control",
        Row::Header(_) => "",
    };
    c.text_mid(Face::Sans, 14, x + 10, cy, TEXT, label_text);
    let slider = |c: &mut Canvas, pct: u32| {
        style::slider(c, x + 10 + 92 + 12, right - 34 - 12, cy, pct, TRACK, if on { ACCENT } else { MUTED }, on);
        let v = if pct == 0 { "Off".to_string() } else { format!("{pct}%") };
        c.text_mid_right(Face::Display, 14, right, cy, if on { TEXT } else { MUTED }, &v);
    };
    let choice = |c: &mut Canvas, v: &str| style::choice(c, right, cy, v, TEXT, MUTED);
    let info = |c: &mut Canvas, v: &str| {
        c.text_mid_right(Face::Sans, 14, right, cy, MUTED, v);
    };
    match row {
        Row::Brightness => slider(c, s.brightness as u32),
        Row::Volume => slider(c, s.volume_pct()),
        Row::Scaling => choice(
            c,
            match s.scaling {
                Scaling::Aspect => "Keep aspect",
                Scaling::Integer => "Integer",
                Scaling::Stretch => "Stretch",
            },
        ),
        Row::Filter => choice(c, if s.filter == Filter::Sharp { "Sharp" } else { "Smooth" }),
        Row::Colour | Row::Contrast | Row::Sharpness => {
            // Steps from normal: -5..+5.
            let d = match row {
                Row::Colour => (s.colour as i32 - 50) / 10,
                Row::Contrast => (s.contrast as i32 - 50) / 10,
                _ => s.sharpness as i32 - 5,
            };
            choice(c, &if d == 0 { "Normal".to_string() } else { format!("{d:+}") })
        }
        Row::Border => choice(
            c,
            match s.border {
                Border::Black => "Black",
                Border::Grey => "Grey",
                Border::Blue => "Blue",
            },
        ),
        Row::Headphones => info(c, if app.headphones { "Plugged in" } else { "Not plugged in" }),
        Row::Battery => info(
            c,
            &match &app.battery {
                Some((cap, st)) => format!("{cap}% · {}", st.to_lowercase()),
                None => "unknown".into(),
            },
        ),
        Row::ScreenOff => {}
        Row::PowerOff => {
            if app.set.confirm_off.is_some() {
                c.text_mid_right(Face::SansBold, 14, right, cy, ACCENT, "Press A again to power off");
            }
        }
        Row::Games => info(c, &format!("{} in {} systems", group(app.lib.total()), app.lib.systems_used())),
        Row::Card => info(
            c,
            &match app.card_space {
                Some((free, total)) => format!("{} free of {}", size_text(free), size_text(total)),
                None => "unknown".into(),
            },
        ),
        Row::Linux => info(c, &app.versions.0),
        Row::Syscore => info(c, &app.versions.1),
        Row::System => info(c, env!("CARGO_PKG_VERSION")),
        Row::Cpu | Row::Memory => {
            let st = &app.stats.now;
            let (pct, text) = if row == Row::Cpu {
                let p = st.cpu.unwrap_or(0);
                (p, match st.cpu {
                    Some(p) => format!("{p}% · 1188 MHz"),
                    None => "… · 1188 MHz".into(),
                })
            } else {
                (st.mem_used_mb * 100 / st.mem_total_mb.max(1), format!("{} of {} MB", st.mem_used_mb, st.mem_total_mb))
            };
            style::slider(c, x + 10 + 92 + 12, right - 150, cy, pct, TRACK, MUTED, false);
            c.text_mid_right(Face::Sans, 14, right, cy, MUTED, &text);
        }
        Row::SecondCore => {
            let load = app.stats.now.second_core.map(|p| format!("{p}% · ")).unwrap_or_default();
            info(c, &format!("{load}{} · firmware {}", app.stats.core, app.versions.1))
        }
        Row::Uptime => {
            let s = app.stats.now.uptime_s;
            info(c, &if s < 60 { format!("{s} s") } else if s < 3600 { format!("{} min", s / 60) } else { format!("{} h {} min", s / 3600, s / 60 % 60) })
        }
        Row::Perf => choice(c, if s.perf { "On" } else { "Off" }),
        Row::Vertical => choice(c, if s.sideways { "Turn the console" } else { "Upright" }),
        Row::Ps1Cores => choice(c, if s.ps1_two_cores { "Both cores" } else { "Main core" }),
        Row::Storage => info(c, "USB drive"),
        Row::UsbControl => choice(c, if app.usb_control { "On" } else { "Off" }),
        Row::Rescan | Row::ExitConsole | Row::Header(_) => {}
    }
}

fn storage(c: &mut Canvas, app: &mut App) {
    c.text_mid(Face::DisplayBold, 20, 16, 50, TEXT, "USB storage");
    c.rect(0, 71, c.w, 1, RULE);
    let Some(s) = &app.storage else { return };
    let (title, lines): (String, Vec<String>) = if let Some(e) = s.state.strip_prefix("error ") {
        ("Could not share the SD card".into(), vec![e.to_string(), "Press B to go back.".into()])
    } else if s.leaving {
        ("Getting the SD card back…".into(), vec![])
    } else if s.state != "on" {
        ("Preparing the SD card…".into(), vec![])
    } else if s.host {
        (
            "Connected to your computer".into(),
            vec![
                "The SD card shows up as a USB drive. Put games in the folder for their system, e.g. PS/, SFC/ or GBA/.".into(),
                "When you're done, eject the drive on the computer, or press B here.".into(),
            ],
        )
    } else {
        (
            "Connect a USB-C cable to your computer".into(),
            vec!["The SD card will show up there as a USB drive. Press B to cancel.".into()],
        )
    };
    let mut y = 150;
    for l in c.ui.wrap(Face::DisplayBold, 24, &title, c.w - 80, 2) {
        c.text_mid_center(Face::DisplayBold, 24, c.w / 2, y, TEXT, &l);
        y += 30;
    }
    y += 14;
    for para in lines {
        for l in c.ui.wrap(Face::Sans, 14, &para, c.w - 120, 4) {
            c.text_mid_center(Face::Sans, 14, c.w / 2, y, MUTED, &l);
            y += 20;
        }
        y += 10;
    }
    let done = if s.state == "on" { "Done" } else { "Back" };
    hint_bar(c, &[(Badge::Button("B", false), done)], "");
}

fn toast(c: &mut Canvas, text: &str, bar: Option<u32>) {
    let tw = c.ui.measure(Face::Sans, 13, text, 0);
    let w = tw.max(if bar.is_some() { 180 } else { 0 }) + 28;
    let h = if bar.is_some() { 52 } else { 36 };
    let (x, y) = ((c.w - w) / 2, c.h - 36 - 14 - h);
    c.round_rect(x, y, w, h, 6, PANEL_EDGE);
    c.round_rect(x + 1, y + 1, w - 2, h - 2, 5, PANEL);
    c.text_mid(Face::Sans, 13, x + 14, y + 18, TEXT, text);
    if let Some(pct) = bar {
        let (bx, bw) = (x + 14, w - 28);
        c.round_rect(bx, y + 34, bw, 6, 3, TRACK);
        c.round_rect(bx, y + 34, (bw * pct.min(100) as i32 / 100).max(6), 6, 3, ACCENT);
    }
}

fn overlay(c: &mut Canvas, kicker: &str, title: &str, note: &str) {
    let (w, h) = (c.w, c.h);
    c.shade(0, 0, w, h, 0x000000, 170);
    let tw = c.ui.measure(Face::DisplayBold, 21, title, 0).min(w - 80);
    let bw = tw.max(c.ui.measure(Face::Sans, 12, note, 0)) + 48;
    let (bx, by, bh) = ((w - bw) / 2, h / 2 - 50, 100);
    c.round_rect(bx, by, bw, bh, 6, PANEL_EDGE);
    c.round_rect(bx + 1, by + 1, bw - 2, bh - 2, 5, BG);
    c.text_mid_center(Face::Sans, 13, w / 2, by + 24, MUTED, kicker);
    let t = c.ui.ellipsize(Face::DisplayBold, 21, title, w - 80);
    c.text_mid_center(Face::DisplayBold, 21, w / 2, by + 50, TEXT, &t);
    if !note.is_empty() {
        c.text_mid_center(Face::Sans, 12, w / 2, by + 76, DIM, note);
    }
}
