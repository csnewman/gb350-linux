//! The in-game menu (hold Select + Start), the volume slider and the stats
//! overlay, drawn on the display's overlay plane (premultiplied ARGB above
//! the game), so the game's own frames are never touched: the menu dims the
//! paused game behind it; while playing, a strip across the top carries the
//! stats (left, when switched on) and the volume slider (right, for 1.5 s).

use crate::{Scale, Video};
use gb350_kms::{Card, Dumb, SetPlane, ARGB8888};
use gb350_ui::input::{Event, Input, Key};
use gb350_ui::style::{self, label, Badge, ACCENT, ACCENT_DARK, BG, DIM, LIST, MUTED, PANEL_EDGE, RULE, TEXT, TRACK};
use gb350_ui::{Canvas, Face, Ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const W: i32 = 640;
const H: i32 = 480;
/// The strip over the running game.
const HUD_H: i32 = 76;
const OSD_W: i32 = 220;
const OSD_H: i32 = 64;
const OSD_TIME: Duration = Duration::from_millis(1500);
/// Volume step for the keys and the menu, percent.
pub const VOLUME_STEP: i32 = gb350_pcm::VOLUME_STEP as i32;
const BACKLIGHT: &str = "/sys/class/backlight/backlight";
const STATE_DIR: &str = "/mnt/sdcard/gb350/retro/states";
pub const SLOTS: u8 = 4;

#[derive(PartialEq, Clone, Copy)]
enum Shown {
    Nothing,
    Menu,
    Hud,
}

/// The volume slider (or a message, with `bar` None).
struct Osd {
    kicker: String,
    text: String,
    bar: Option<u32>,
    until: Instant,
}

/// The overlay plane and what is on it.
pub struct Overlay {
    plane: u32,
    ui: Ui,
    menu_fb: Dumb,
    hud_fb: Dumb,
    scratch: Vec<u32>,
    shown: Shown,
    osd: Option<Osd>,
    /// The stats overlay is switched on (settings `perf`).
    pub perf_on: bool,
    /// Its current text.
    perf: Option<String>,
}

impl Overlay {
    pub fn new(card: &Card, perf_on: bool) -> Result<Overlay, String> {
        let plane = *card.planes.get(1).ok_or("no overlay plane")?;
        let mut menu_fb = card.dumb(W as u32, H as u32, ARGB8888, 32)?;
        let mut hud_fb = card.dumb(W as u32, HUD_H as u32, ARGB8888, 32)?;
        menu_fb.pixels().fill(0);
        hud_fb.pixels().fill(0);
        Ok(Overlay { plane, ui: Ui::new(), menu_fb, hud_fb, scratch: vec![0; (W * H) as usize], shown: Shown::Nothing, osd: None, perf_on, perf: None })
    }

    /// Draws with `f` into the menu (full screen) or strip buffer.
    fn paint(&mut self, which: Shown, f: impl FnOnce(&mut Canvas)) {
        let (w, h) = if which == Shown::Menu { (W, H) } else { (W, HUD_H) };
        let px = &mut self.scratch[..(w * h) as usize];
        f(&mut Canvas::new(px, w, h, w as usize, &mut self.ui));
        // Drawn off screen, then copied (the buffer may be on screen).
        let fb = if which == Shown::Menu { &mut self.menu_fb } else { &mut self.hud_fb };
        let pitch = fb.pitch as usize / 4;
        let bytes = fb.pixels();
        let dst = unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, bytes.len() / 4) };
        for y in 0..h as usize {
            dst[y * pitch..y * pitch + w as usize].copy_from_slice(&self.scratch[y * w as usize..(y + 1) * w as usize]);
        }
    }

    fn show(&mut self, card: &Card, which: Shown) {
        if self.shown == which {
            return;
        }
        let (fb, h) = match which {
            Shown::Menu => (&self.menu_fb, H),
            Shown::Hud => (&self.hud_fb, HUD_H),
            Shown::Nothing => return self.hide(card),
        };
        let mut sp = SetPlane {
            plane_id: self.plane,
            crtc_id: card.crtc,
            fb_id: fb.id,
            crtc_w: W as u32,
            crtc_h: h as u32,
            src_w: (W as u32) << 16,
            src_h: (h as u32) << 16,
            ..Default::default()
        };
        match card.set_plane(&mut sp) {
            Ok(()) => self.shown = which,
            Err(e) => eprintln!("retro: overlay: {e}"),
        }
    }

    pub fn hide(&mut self, card: &Card) {
        if self.shown == Shown::Nothing {
            return;
        }
        let mut sp = SetPlane { plane_id: self.plane, ..Default::default() };
        if let Err(e) = card.set_plane(&mut sp) {
            eprintln!("retro: overlay off: {e}");
        }
        self.shown = Shown::Nothing;
    }

    /// Redraws the strip over the game (or hides it when empty).
    pub fn refresh_hud(&mut self, card: &Card) {
        let perf = if self.perf_on { self.perf.clone() } else { None };
        if perf.is_none() && self.osd.is_none() {
            return self.hide(card);
        }
        let osd = self.osd.as_ref().map(|o| (o.kicker.clone(), o.text.clone(), o.bar));
        self.paint(Shown::Hud, |c| {
            c.clear();
            if let Some(p) = &perf {
                let w = c.ui.measure(Face::Mono, 12, p, 0) + 20;
                c.round_rect(12, 12, w, 24, 6, BG);
                c.text_mid(Face::Mono, 12, 22, 24, TEXT, p);
            }
            if let Some((kicker, text, bar)) = &osd {
                let x = W - 12 - OSD_W;
                c.round_rect(x, 12, OSD_W, OSD_H, 6, PANEL_EDGE);
                c.round_rect(x + 1, 13, OSD_W - 2, OSD_H - 2, 5, BG);
                label(c, x + 14, 31, kicker);
                match bar {
                    Some(pct) => {
                        c.text_mid_right(Face::Display, 14, x + OSD_W - 14, 31, TEXT, &format!("{pct}%"));
                        c.icon("speaker18", x + 12, 45, MUTED, || style::speaker(18));
                        style::slider(c, x + 38, x + OSD_W - 14, 54, *pct, TRACK, ACCENT, false);
                    }
                    None => {
                        let t = c.ui.ellipsize(Face::Sans, 14, text, OSD_W - 28);
                        c.text_mid(Face::Sans, 14, x + 14, 54, TEXT, &t);
                    }
                }
            }
        });
        self.show(card, Shown::Hud);
    }

    /// Shows the volume slider (or a message, with `bar` None) for 1.5 s.
    pub fn osd(&mut self, card: &Card, kicker: &str, text: &str, bar: Option<u32>) {
        self.osd = Some(Osd { kicker: kicker.into(), text: text.into(), bar, until: Instant::now() + OSD_TIME });
        self.refresh_hud(card);
    }

    /// New stats text (shown if switched on).
    pub fn set_perf(&mut self, card: &Card, text: String) {
        self.perf = Some(text);
        if self.perf_on {
            self.refresh_hud(card);
        }
    }

    /// Blends what the overlay shows onto `screen` (640x480 XRGB), for
    /// test screenshots.
    pub fn composite(&mut self, screen: &mut [u32]) {
        let (fb, h) = match self.shown {
            Shown::Menu => (&mut self.menu_fb, H),
            Shown::Hud => (&mut self.hud_fb, HUD_H),
            Shown::Nothing => return,
        };
        let pitch = fb.pitch as usize / 4;
        let bytes = fb.pixels();
        let src = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, bytes.len() / 4) };
        for y in 0..h {
            for x in 0..W {
                let p = src[y as usize * pitch + x as usize];
                let d = &mut screen[(y * W + x) as usize];
                // Premultiplied source-over.
                let ia = 255 - (p >> 24);
                let ch = |sh: u32| (((p >> sh) & 255) + ((*d >> sh) & 255) * ia / 255).min(255) << sh;
                *d = ch(16) | ch(8) | ch(0);
            }
        }
    }

    /// Takes the slider away once its time is up.
    pub fn tick(&mut self, card: &Card) {
        if self.osd.as_ref().is_some_and(|o| Instant::now() >= o.until) {
            self.osd = None;
            self.refresh_hud(card);
        }
    }
}

/// What the main loop does when the menu closes.
pub enum Action {
    Resume,
    Quit,
    Save(u8),
    Load(u8),
    Reset,
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Resume,
    Save,
    Load,
    Reset,
    Volume,
    Brightness,
    Scaling,
    Filter,
    Stats,
    Cores,
    Quit,
}

const ITEMS: [Item; 10] =
    [Item::Resume, Item::Save, Item::Load, Item::Reset, Item::Volume, Item::Brightness, Item::Scaling, Item::Filter, Item::Stats, Item::Quit];

/// A divider goes above these.
fn divider_before(i: Item) -> bool {
    matches!(i, Item::Volume | Item::Quit)
}

pub struct Menu {
    title: String,
    system: String,
    stem: String,
    /// The launcher's settings file (RETRO_SETTINGS), updated on close.
    settings: Option<PathBuf>,
    sel: usize,
    pub slot: u8,
    confirm_reset: Option<Instant>,
    items: Vec<Item>,
    /// PlayStation: drawing shared with the second core (Some: the menu has
    /// the row; the main loop applies it after the menu closes).
    pub cores: Option<bool>,
}


fn brightness() -> (u32, u32) {
    let read = |f: &str| std::fs::read_to_string(format!("{BACKLIGHT}/{f}")).ok().and_then(|s| s.trim().parse().ok());
    (read("brightness").unwrap_or(153), read("max_brightness").unwrap_or(255).max(1))
}

fn brightness_pct() -> u32 {
    let (b, max) = brightness();
    ((b * 100 + max / 2) / max).clamp(10, 100)
}

fn set_brightness_pct(pct: u32) {
    let (_, max) = brightness();
    let _ = std::fs::write(format!("{BACKLIGHT}/brightness"), (max * pct / 100).to_string());
}

/// A key of the launcher's settings file (RETRO_SETTINGS), if set.
pub fn read_setting(key: &str) -> Option<String> {
    let path = std::env::var("RETRO_SETTINGS").ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().filter_map(|l| l.split_once('=')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string())
}

pub fn state_path(stem: &str, slot: u8) -> String {
    format!("{STATE_DIR}/{stem}.{slot}.state")
}

/// Rewrites `key=value` lines of the launcher's settings file.
pub fn save_settings(path: &Option<PathBuf>, values: &[(&str, String)]) {
    let Some(path) = path else { return };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // Also drops keys of the old volume scheme (gain + speaker, replaced by
    // volume_pct).
    let drop = |k: &str| values.iter().any(|(v, _)| *v == k) || (k == "volume" || k == "speaker") && values.iter().any(|(v, _)| *v == "volume_pct");
    let mut lines: Vec<String> = text.lines().filter(|l| !drop(l.split('=').next().unwrap_or("").trim())).map(String::from).collect();
    lines.extend(values.iter().map(|(k, v)| format!("{k}={v}")));
    let _ = std::fs::write(path, lines.join("\n") + "\n");
}

impl Menu {
    pub fn new(game: &str, system: &str) -> Menu {
        let stem = std::path::Path::new(game).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = std::fs::create_dir_all(STATE_DIR);
        Menu {
            title: std::env::var("RETRO_TITLE").unwrap_or_else(|_| stem.clone()),
            system: system.to_uppercase(),
            stem,
            settings: std::env::var("RETRO_SETTINGS").ok().map(PathBuf::from),
            sel: 0,
            slot: 1,
            confirm_reset: None,
            items: ITEMS.to_vec(),
            cores: None,
        }
    }

    /// Adds the "PS1 drawing" row (before Quit), showing `on`.
    pub fn with_cores(&mut self, on: bool) {
        self.cores = Some(on);
        if !self.items.contains(&Item::Cores) {
            let quit = self.items.len() - 1;
            self.items.insert(quit, Item::Cores);
        }
    }

    pub fn settings_path(&self) -> &Option<PathBuf> {
        &self.settings
    }

    pub fn stem(&self) -> &str {
        &self.stem
    }

    fn slot_used(&self) -> bool {
        std::path::Path::new(&state_path(&self.stem, self.slot)).exists()
    }

    /// Runs the menu over the paused game until it closes. `volume` (percent) is the
    /// volume, changed in place.
    pub fn run(&mut self, video: &mut Video, input: &mut Input, volume: &mut i32, headphones: bool) -> Action {
        // Out of `video` while the menu runs, so the game picture can be
        // re-placed as scaling changes.
        let Some(mut ov) = video.overlay.take() else { return Action::Resume };
        self.sel = 0;
        self.confirm_reset = None;
        input.resync();
        let battery = std::fs::read_to_string("/sys/class/power_supply/battery/capacity").ok().map(|s| s.trim().to_string());
        let mut dirty = true;
        let mut events = vec![];
        // Tests: RETRO_MENU_KEYS="down,right,shot:/tmp/m.ppm,b" drives the
        // menu instead of the buttons.
        let mut script: Vec<String> = std::env::var("RETRO_MENU_KEYS")
            .map(|v| v.split(',').map(|k| k.trim().to_string()).rev().collect())
            .unwrap_or_default();
        let action = 'menu: loop {
            if dirty {
                let (scale, sharp) = (video.scale, video.sharp);
                let perf = ov.perf_on;
                ov.paint(Shown::Menu, |c| self.draw(c, *volume, scale, sharp, perf, battery.as_deref(), headphones));
                ov.show(&video.card, Shown::Menu);
                dirty = false;
            }
            let now = Instant::now();
            let mut deadline = now + Duration::from_secs(10);
            if let Some(d) = input.deadline() {
                deadline = deadline.min(d);
            }
            if let Some(c) = self.confirm_reset {
                deadline = deadline.min(c + Duration::from_secs(3));
            }
            let mut fds: Vec<libc::pollfd> = input.fds().into_iter().map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
            let timeout = if script.is_empty() { deadline.saturating_duration_since(now).as_millis() as i32 } else { 50 };
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            events.clear();
            input.read(&mut events);
            if let Some(step) = script.pop() {
                if let Some(path) = step.strip_prefix("shot:") {
                    let mut screen = crate::screen_rgb(video);
                    ov.composite(&mut screen);
                    crate::write_ppm(path, &screen);
                } else if let Some(k) = Key::parse(&step) {
                    events.push(Event::Press(k));
                }
            }
            input.tick(Instant::now(), &mut events);
            if self.confirm_reset.is_some_and(|c| c.elapsed() >= Duration::from_secs(3)) {
                self.confirm_reset = None;
                dirty = true;
            }
            for e in events.drain(..) {
                let Event::Press(k) = e else { continue };
                dirty = true;
                let item = self.items[self.sel];
                if item != Item::Reset || k != Key::A {
                    self.confirm_reset = None;
                }
                match k {
                    Key::Up => self.sel = (self.sel + self.items.len() - 1) % self.items.len(),
                    Key::Down => self.sel = (self.sel + 1) % self.items.len(),
                    Key::B | Key::Start => break 'menu Action::Resume,
                    Key::VolUp | Key::VolDown => {
                        *volume = (*volume + if k == Key::VolUp { VOLUME_STEP } else { -VOLUME_STEP }).clamp(0, 100);
                        set_volume(*volume);
                    }
                    Key::Left | Key::Right => {
                        let up = k == Key::Right;
                        match item {
                            Item::Save | Item::Load => self.slot = if up { self.slot % SLOTS + 1 } else { (self.slot + SLOTS - 2) % SLOTS + 1 },
                            Item::Volume => {
                                *volume = (*volume + if up { VOLUME_STEP } else { -VOLUME_STEP }).clamp(0, 100);
                                set_volume(*volume);
                            }
                            Item::Brightness => {
                                let p = brightness_pct() as i32 + if up { 10 } else { -10 };
                                set_brightness_pct(p.clamp(10, 100) as u32);
                            }
                            Item::Scaling => {
                                video.scale = match (video.scale, up) {
                                    (Scale::Aspect, true) | (Scale::Stretch, false) => Scale::Integer,
                                    (Scale::Integer, true) | (Scale::Aspect, false) => Scale::Stretch,
                                    _ => Scale::Aspect,
                                };
                                video.replace();
                            }
                            Item::Filter => {
                                video.sharp = !video.sharp;
                                video.replace();
                            }
                            Item::Stats => ov.perf_on = !ov.perf_on,
                            Item::Cores => self.cores = self.cores.map(|c| !c),
                            _ => {}
                        }
                    }
                    Key::A => match item {
                        Item::Resume => break 'menu Action::Resume,
                        Item::Save => break 'menu Action::Save(self.slot),
                        Item::Load if self.slot_used() => break 'menu Action::Load(self.slot),
                        Item::Reset if self.confirm_reset.is_some() => break 'menu Action::Reset,
                        Item::Reset => self.confirm_reset = Some(Instant::now()),
                        Item::Quit => break 'menu Action::Quit,
                        Item::Filter => {
                            video.sharp = !video.sharp;
                            video.replace();
                        }
                        Item::Stats => ov.perf_on = !ov.perf_on,
                        Item::Cores => self.cores = self.cores.map(|c| !c),
                        _ => {}
                    },
                    _ => {}
                }
            }
        };
        // Back to the strip over the game (stats, if on), or nothing.
        ov.refresh_hud(&video.card);
        let perf = ov.perf_on;
        video.overlay = Some(ov);
        let mut values = vec![
            ("volume_pct", volume.to_string()),
            ("brightness", brightness_pct().to_string()),
            ("scaling", video.scale.name().to_string()),
            ("filter", (if video.sharp { "sharp" } else { "smooth" }).to_string()),
            ("perf", (if perf { "on" } else { "off" }).to_string()),
        ];
        if let Some(c) = self.cores {
            values.push(("ps1_cores", (if c { "2" } else { "1" }).to_string()));
        }
        save_settings(&self.settings, &values);
        action
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(&self, c: &mut Canvas, volume: i32, scale: Scale, sharp: bool, perf: bool, battery: Option<&str>, _headphones: bool) {
        c.clear();
        // The paused game shows through, dimmed, beside the panel.
        let pw = 276;
        c.shade(pw, 0, W - pw, H, 0x000000, 158);
        c.rect(0, 0, pw, H, BG);
        c.rect(pw, 0, 1, H, RULE);
        label(c, 22, 26, &format!("PAUSED · {}", self.system));
        let t = c.ui.ellipsize(Face::DisplayBold, 21, &self.title, pw - 40);
        c.text_mid(Face::DisplayBold, 21, 22, 50, TEXT, &t);

        let mut y = 72;
        // Eleven rows (with "PS1 drawing") fit above the help at 30 px.
        let step = if self.items.len() > ITEMS.len() { 30 } else { 32 };
        for (i, &item) in self.items.iter().enumerate() {
            if divider_before(item) {
                c.rect(22, y + 3, pw - 44, 1, RULE);
                y += 9;
            }
            let on = i == self.sel;
            let (face, fg, sub) = if on { (Face::SansBold, BG, BG) } else { (Face::Sans, LIST, MUTED) };
            if on {
                c.round_rect(12, y, pw - 24, 30, 4, ACCENT);
            }
            let cy = y + 15;
            let right = pw - 22;
            let name = match item {
                Item::Resume => "Resume",
                Item::Save => "Save state",
                Item::Load => "Load state",
                Item::Reset => "Reset game",
                Item::Volume => "Volume",
                Item::Brightness => "Brightness",
                Item::Scaling => "Scaling",
                Item::Filter => "Filter",
                Item::Stats => "Stats overlay",
                Item::Cores => "PS1 drawing",
                Item::Quit => "Quit to library",
            };
            c.text_mid(face, 15, 22, cy, fg, name);
            let level = |c: &mut Canvas, pct: u32| {
                let (track, fill) = if on { (ACCENT_DARK, BG) } else { (style::TRACK, MUTED) };
                style::slider(c, 22 + 96, right - 40, cy, pct, track, fill, false);
                let v = if pct == 0 { "Off".to_string() } else { format!("{pct}%") };
                c.text_mid_right(Face::Display, 13, right, cy, sub, &v);
            };
            match item {
                Item::Save => style::choice(c, right, cy, &format!("Slot {}", self.slot), fg, sub),
                Item::Load => {
                    let v = if self.slot_used() { format!("Slot {}", self.slot) } else { format!("Slot {} empty", self.slot) };
                    c.text_mid_right(Face::Sans, 13, right, cy, sub, &v);
                }
                Item::Reset if self.confirm_reset.is_some() => {
                    c.text_mid_right(Face::SansBold, 13, right, cy, if on { BG } else { ACCENT }, "Press A again");
                }
                Item::Volume => level(c, volume as u32),
                Item::Brightness => level(c, brightness_pct()),
                Item::Scaling => style::choice(c, right, cy, scale.label(), fg, sub),
                Item::Filter => style::choice(c, right, cy, if sharp { "Sharp" } else { "Smooth" }, fg, sub),
                Item::Stats => style::choice(c, right, cy, if perf { "On" } else { "Off" }, fg, sub),
                Item::Cores => style::choice(c, right, cy, if self.cores == Some(true) { "Both cores" } else { "Main core" }, fg, sub),
                _ => {}
            }
            y += step;
        }

        // Help for the selected item, at the bottom of the panel.
        let help = match self.items[self.sel] {
            Item::Resume => "Back to the game. B or START also resumes.".to_string(),
            Item::Save => format!("Saves the game as it is now. Left / Right picks one of {SLOTS} slots."),
            Item::Load if self.slot_used() => "Goes back to the saved slot. Left / Right picks the slot.".to_string(),
            Item::Load => format!("Nothing saved in slot {} yet.", self.slot),
            Item::Reset => "Restarts the game; progress since the last save is lost.".to_string(),
            Item::Volume => "0 % turns the speaker off. The volume keys work while playing too.".to_string(),
            Item::Brightness => "Screen backlight.".to_string(),
            Item::Scaling => "Integer keeps every pixel the same size; stretch fills the screen.".to_string(),
            Item::Filter => "Smooth blends pixels as they are scaled up; sharp keeps hard edges.".to_string(),
            Item::Stats => "Frames per second, CPU, second core and memory use at the top while playing.".to_string(),
            Item::Cores => "Both cores share the drawing (faster in heavy 3D), or the main core draws it all.".to_string(),
            Item::Quit => "Back to the library. The memory card is saved.".to_string(),
        };
        let lines = c.ui.wrap(Face::Sans, 12, &help, pw - 44, 2);
        let mut ly = H - 14 - lines.len() as i32 * 17;
        for l in lines {
            c.text_mid(Face::Sans, 12, 22, ly + 8, DIM, &l);
            ly += 17;
        }

        if let Some(b) = battery {
            let s = format!("Battery {b}%");
            let w = c.ui.measure(Face::Sans, 12, &s, 0) + 20;
            c.round_rect(W - 16 - w, 16, w, 26, 6, BG);
            c.text_mid(Face::Sans, 12, W - 16 - w + 10, 29, MUTED, &s);
        }
        style::hint_bar(c, pw + 1, W, &[(Badge::Button("A", true), "Select"), (Badge::Button("B", false), "Resume"), (Badge::Arrows, "Change")], "");
    }
}

fn set_volume(pct: i32) {
    if let Err(e) = gb350_pcm::set_volume(pct.clamp(0, 100) as u8) {
        eprintln!("retro: volume: {e}");
    }
}

