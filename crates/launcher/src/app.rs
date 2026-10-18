//! Launcher state and what each button does on each screen.

use gb350_ui::input::Key;
use crate::library::{self, Library, Tab, SYSTEMS};
use crate::preview::Previews;
use crate::settings::{self, Border, Filter, Scaling, Settings, VOLUME_STEP};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Library,
    Search,
    Settings,
    /// The SD card is a USB drive on a computer.
    Storage,
}

/// Requests to the agent (PID 1): a line in this file, then SIGUSR1.
const AGENT_REQUEST: &str = "/tmp/gb350-agent.request";
/// The agent's report on USB storage mode (its storage.rs).
const STORAGE_STATE: &str = "/tmp/gb350-storage.state";

fn ask_agent(line: &str) {
    let _ = std::fs::write(AGENT_REQUEST, line);
    unsafe { libc::kill(1, libc::SIGUSR1) };
}

pub struct StorageState {
    /// The agent's report: "on", "off", "error ..." (empty: no answer yet).
    pub state: String,
    /// A computer has the USB connection configured.
    pub host: bool,
    /// The card was handed over (so "off" means it came back).
    pub was_on: bool,
    pub leaving: bool,
}

/// Rows of the library list on screen.
pub const LIST_ROWS: usize = 10;
/// Rows of search results on screen.
pub const RESULT_ROWS: usize = 10;
const SEARCH_MAX: usize = 300;

/// The on-screen keyboard: 7 columns; the last row is 9, space, delete.
pub const KEY_COLS: usize = 7;
pub const KEY_CHARS: &str = "abcdefghijklmnopqrstuvwxyz012345678";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyCap {
    Char(char),
    Space,
    Delete,
}

/// (column, row, span, cap) of every key.
pub fn keyboard() -> Vec<(usize, usize, usize, KeyCap)> {
    let mut v: Vec<_> = KEY_CHARS.chars().enumerate().map(|(i, c)| (i % KEY_COLS, i / KEY_COLS, 1, KeyCap::Char(c))).collect();
    let r = KEY_CHARS.len() / KEY_COLS;
    v.push((0, r, 1, KeyCap::Char('9')));
    v.push((1, r, 3, KeyCap::Space));
    v.push((4, r, 3, KeyCap::Delete));
    v
}

pub struct SearchState {
    pub query: String,
    /// Index into `keyboard()`.
    pub key: usize,
    pub in_results: bool,
    pub results: Vec<u32>,
    pub sel: usize,
    pub top: usize,
    /// Limit to one system.
    pub filter: Option<usize>,
}

/// Settings rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Header(&'static str),
    Brightness,
    Scaling,
    Filter,
    Volume,
    Headphones,
    Battery,
    PowerOff,
    Games,
    Rescan,
    Card,
    Storage,
    Linux,
    Syscore,
    System,
    UsbControl,
    ExitConsole,
    Vertical,
    Ps1Cores,
    Cpu,
    Memory,
    SecondCore,
    Uptime,
    Perf,
    Colour,
    Contrast,
    Sharpness,
    Border,
    ScreenOff,
}

pub const SECTIONS: &[&str] = &["Display & sound", "Power & battery", "Library & storage", "System", "About"];
/// The section with live figures.
pub const SYSTEM_SECTION: usize = 3;

pub fn rows(section: usize) -> &'static [Row] {
    use Row::*;
    match section {
        0 => &[
            Header("DISPLAY"),
            Brightness,
            Scaling,
            Filter,
            Vertical,
            Header("PICTURE"),
            Colour,
            Contrast,
            Sharpness,
            Border,
            Header("SOUND"),
            Volume,
            Headphones,
        ],
        1 => &[Header("BATTERY"), Battery, Header("POWER"), ScreenOff, PowerOff],
        2 => &[Header("LIBRARY"), Games, Rescan, Header("STORAGE"), Card, Storage],
        3 => &[Header("NOW"), Cpu, Memory, SecondCore, Uptime, Header("IN GAMES"), Perf, Ps1Cores],
        _ => &[Header("VERSIONS"), Linux, Syscore, System, Header("DEVELOPER"), UsbControl, ExitConsole],
    }
}

impl Row {
    pub fn selectable(self) -> bool {
        matches!(
            self,
            Row::Brightness
                | Row::Scaling
                | Row::Filter
                | Row::Volume
                                | Row::PowerOff
                | Row::Rescan
                | Row::Storage
                | Row::UsbControl
                | Row::ExitConsole
                | Row::Perf
                | Row::Vertical
                | Row::Ps1Cores
                | Row::Colour
                | Row::Contrast
                | Row::Sharpness
                | Row::Border
                | Row::ScreenOff
        )
    }

    pub fn action(self) -> bool {
        matches!(self, Row::PowerOff | Row::Rescan | Row::Storage | Row::ExitConsole | Row::ScreenOff)
    }

    pub fn help(self) -> &'static str {
        match self {
            Row::Brightness => "Screen backlight.",
            Row::Scaling => "How games fill the screen. Integer keeps every pixel the same size; stretch fills the panel.",
            Row::Filter => "Smooth blends pixels when a game is scaled up; sharp keeps hard pixel edges.",
            Row::Volume => "0 % turns the speaker off. Plugged-in headphones take over from the speaker. The volume keys change it anywhere, in games too.",
            Row::PowerOff => "Holding the power button for 8 seconds also turns the console off.",
            Row::Rescan => "Reads the SD card again after adding or removing games.",
            Row::Storage => "Shows the SD card on a computer as a USB drive, to copy games onto it. Games can't be played meanwhile.",
            Row::Cpu => "Linux's core, at a fixed 1188 MHz. The second core handles buttons, battery and power on its own.",
            Row::Memory => "In use by programs and the display; the rest is free or cache.",
            Row::SecondCore => "The second processor's firmware (buttons, battery, power, speaker).",
            Row::Vertical => "Games made for a tall screen: upright on the wide screen (smaller), or sideways at full size - turn the console, the D-pad turns with it.",
            Row::Perf => "Shows frames per second, CPU, second-core and memory use at the top while playing. The in-game menu switches it too.",
            Row::Ps1Cores => "PlayStation games: the second core draws part of each picture alongside the main core (faster in heavy 3D scenes), or the main core draws it all. From the next game started.",
            Row::UsbControl => "Lets a computer on the USB cable run commands and change files on the console (gbctl). For development; leave it off otherwise.",
            Row::ExitConsole => "Shows the log of what the system reported. START brings the launcher back.",
            Row::Colour => "How vivid colours are, in games and here.",
            Row::Contrast => "The difference between dark and light.",
            Row::Sharpness => "Crisper or softer edges, done by the display hardware.",
            Row::Border => "The colour around a game that doesn't fill the screen.",
            Row::ScreenOff => "Turns the screen off to save power; any button turns it back on.",
            _ => "",
        }
    }
}

pub struct SettingsState {
    pub section: usize,
    pub row: usize,
    /// Pixels the row list is scrolled by (when a section is taller than
    /// the panel; the view keeps the selected row in sight).
    pub scroll: i32,
    /// First press of A on Power off: asks again until then.
    pub confirm_off: Option<Instant>,
}

pub struct Toast {
    pub text: String,
    /// A level bar (volume), percent.
    pub bar: Option<u32>,
    pub until: Instant,
}

/// What the main loop must do after a key.
pub enum Action {
    None,
    Launch(u32),
    PowerOff,
    ScreenOff,
    Exit,
}

pub struct App {
    pub lib: Library,
    pub settings: Settings,
    pub previews: Previews,
    pub screen: Screen,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    /// First tab shown in the tab strip.
    pub tab_scroll: usize,
    pub list: Vec<u32>,
    pub sel: usize,
    pub top: usize,
    cursors: HashMap<Tab, (usize, usize)>,
    pub search: SearchState,
    pub set: SettingsState,
    pub toast: Option<Toast>,
    pub battery: Option<(u32, String)>,
    battery_at: Instant,
    pub headphones: bool,
    /// Title shown while a game starts.
    pub loading: Option<String>,
    pub powering_off: bool,
    pub card_space: Option<(u64, u64)>,
    pub storage: Option<StorageState>,
    /// The agent's USB control channel (gb350/agent.conf).
    pub usb_control: bool,
    /// Settings -> System, while it is open.
    pub stats: settings::Stats,
    sampler: settings::Sampler,
    stats_at: Instant,
    pub versions: (String, String),
}

impl App {
    pub fn new(lib: Library, settings: Settings, previews: Previews) -> App {
        let mut app = App {
            lib,
            settings,
            previews,
            screen: Screen::Library,
            tabs: vec![],
            tab: 0,
            tab_scroll: 0,
            list: vec![],
            sel: 0,
            top: 0,
            cursors: HashMap::new(),
            search: SearchState { query: String::new(), key: 0, in_results: false, results: vec![], sel: 0, top: 0, filter: None },
            set: SettingsState { section: 0, row: 1, scroll: 0, confirm_off: None },
            toast: None,
            battery: settings::battery(),
            battery_at: Instant::now(),
            headphones: false,
            loading: None,
            powering_off: false,
            card_space: None,
            storage: None,
            usb_control: false,
            stats: settings::Stats::default(),
            sampler: settings::Sampler::default(),
            stats_at: Instant::now(),
            versions: (settings::kernel_release(), settings::syscore_version()),
        };
        app.refresh_tabs(None);
        app
    }

    pub fn current_tab(&self) -> Option<Tab> {
        self.tabs.get(self.tab).copied()
    }

    /// Rebuilds the tab list (after playing, favouriting, rescanning),
    /// staying on `keep` (or the current tab) where possible.
    pub fn refresh_tabs(&mut self, keep: Option<Tab>) {
        let keep = keep.or(self.current_tab());
        self.tabs = self.lib.tabs();
        // Start on the first system rather than recent/favourites.
        let first_sys = self.tabs.iter().position(|t| matches!(t, Tab::System(_))).unwrap_or(0);
        self.tab = keep.and_then(|k| self.tabs.iter().position(|&t| t == k)).unwrap_or(first_sys);
        self.load_list();
    }

    fn load_list(&mut self) {
        self.list = self.current_tab().map(|t| self.lib.list(t)).unwrap_or_default();
        let (sel, top) = self.current_tab().and_then(|t| self.cursors.get(&t).copied()).unwrap_or((0, 0));
        self.sel = sel.min(self.list.len().saturating_sub(1));
        self.top = top;
        self.fix_scroll();
    }

    fn save_cursor(&mut self) {
        if let Some(t) = self.current_tab() {
            self.cursors.insert(t, (self.sel, self.top));
        }
    }

    fn fix_scroll(&mut self) {
        let n = self.list.len();
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + LIST_ROWS {
            self.top = self.sel + 1 - LIST_ROWS;
        }
        self.top = self.top.min(n.saturating_sub(LIST_ROWS));
    }

    pub fn selected(&self) -> Option<u32> {
        match self.screen {
            Screen::Search => self.search.results.get(self.search.sel).copied(),
            _ => self.list.get(self.sel).copied(),
        }
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), bar: None, until: Instant::now() + Duration::from_millis(2500) });
    }

    /// The next time something on screen changes by itself.
    pub fn deadline(&self) -> Instant {
        let mut d = self.battery_at + Duration::from_secs(30);
        if let Some(t) = &self.toast {
            d = d.min(t.until);
        }
        if let Some(c) = self.set.confirm_off {
            d = d.min(c + Duration::from_secs(4));
        }
        if self.storage.is_some() {
            d = d.min(Instant::now() + Duration::from_millis(300));
        }
        if self.live_stats() {
            d = d.min(self.stats_at + Duration::from_secs(1));
        }
        d
    }

    fn live_stats(&self) -> bool {
        self.screen == Screen::Settings && self.set.section == SYSTEM_SECTION
    }

    /// Timers; true if the screen changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        if self.toast.as_ref().is_some_and(|t| now >= t.until) {
            self.toast = None;
            dirty = true;
        }
        if self.set.confirm_off.is_some_and(|c| now >= c + Duration::from_secs(4)) {
            self.set.confirm_off = None;
            dirty = true;
        }
        if self.storage.is_some() {
            dirty |= self.poll_storage();
        }
        if self.live_stats() && now >= self.stats_at + Duration::from_secs(1) {
            self.stats = self.sampler.sample();
            self.stats_at = now;
            dirty = true;
        }
        if now >= self.battery_at + Duration::from_secs(30) {
            self.battery_at = now;
            let b = settings::battery();
            dirty |= b != self.battery;
            self.battery = b;
        }
        dirty
    }

    pub fn key(&mut self, k: Key) -> Action {
        match k {
            Key::VolUp | Key::VolDown => {
                let v = self.settings.volume;
                self.settings.volume = if k == Key::VolUp { (v + VOLUME_STEP).min(100) } else { v.saturating_sub(VOLUME_STEP) };
                self.settings.apply_volume();
                self.settings.save();
                let pct = self.settings.volume_pct();
                let text = if pct == 0 { "Volume 0% · muted".to_string() } else { format!("Volume {pct}%") };
                self.toast = Some(Toast { text, bar: Some(pct), until: Instant::now() + Duration::from_millis(1500) });
                return Action::None;
            }
            Key::Power if self.screen != Screen::Storage => {
                self.open_settings(1);
                self.set.row = rows(1).iter().position(|&r| r == Row::PowerOff).unwrap_or(0);
                return Action::None;
            }
            _ => {}
        }
        match self.screen {
            Screen::Library => self.library_key(k),
            Screen::Search => self.search_key(k),
            Screen::Settings => self.settings_key(k),
            Screen::Storage => {
                if k == Key::B {
                    self.storage_leave();
                }
                Action::None
            }
        }
    }

    fn library_key(&mut self, k: Key) -> Action {
        let n = self.list.len();
        match k {
            Key::Up => self.sel = self.sel.saturating_sub(1),
            Key::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            Key::Left => self.sel = self.sel.saturating_sub(LIST_ROWS - 1),
            Key::Right => self.sel = (self.sel + LIST_ROWS - 1).min(n.saturating_sub(1)),
            Key::L1 | Key::R1 if !self.tabs.is_empty() => {
                self.save_cursor();
                let t = self.tabs.len();
                self.tab = if k == Key::R1 { (self.tab + 1) % t } else { (self.tab + t - 1) % t };
                self.load_list();
                return Action::None;
            }
            Key::L2 | Key::R2 if n > 0 => self.jump_letter(k == Key::R2),
            Key::A => return self.selected().map_or(Action::None, Action::Launch),
            Key::X => self.open_search(),
            Key::Y => {
                if let Some(g) = self.selected() {
                    self.favourite(g);
                }
            }
            Key::Start => self.open_settings(0),
            _ => {}
        }
        self.fix_scroll();
        Action::None
    }

    fn jump_letter(&mut self, forward: bool) {
        if self.current_tab() == Some(Tab::Recent) {
            return;
        }
        let letter = |i: usize| self.lib.games[self.list[i] as usize].letter;
        let cur = letter(self.sel);
        if forward {
            if let Some(i) = (self.sel..self.list.len()).find(|&i| letter(i) != cur) {
                self.sel = i;
            }
        } else {
            // Start of this group, or of the previous one if already there.
            let start = (0..=self.sel).rev().take_while(|&i| letter(i) == cur).last().unwrap_or(self.sel);
            if start < self.sel {
                self.sel = start;
            } else if start > 0 {
                let prev = letter(start - 1);
                self.sel = (0..start).rev().take_while(|&i| letter(i) == prev).last().unwrap_or(start - 1);
            }
        }
        // Show the group at the top of the list.
        self.top = self.sel;
    }

    fn favourite(&mut self, g: u32) {
        self.lib.toggle_favourite(g);
        let name = &self.lib.games[g as usize].name;
        let msg = if self.lib.is_favourite(g) { format!("Added {name} to favourites") } else { format!("Removed {name} from favourites") };
        self.toast(msg);
        if self.screen == Screen::Library {
            self.save_cursor();
            self.refresh_tabs(None);
        } else {
            self.tabs = self.lib.tabs();
        }
    }

    fn open_search(&mut self) {
        self.screen = Screen::Search;
        self.search.in_results = false;
    }

    fn run_search(&mut self) {
        self.search.results = library::search(&self.lib, &self.search.query, self.search.filter, SEARCH_MAX);
        self.search.sel = 0;
        self.search.top = 0;
    }

    fn search_key(&mut self, k: Key) -> Action {
        let s = &mut self.search;
        match k {
            Key::L1 | Key::R1 => {
                let systems: Vec<usize> = (0..SYSTEMS.len()).filter(|&i| !self.lib.by_sys[i].is_empty()).collect();
                let mut opts: Vec<Option<usize>> = vec![None];
                opts.extend(systems.into_iter().map(Some));
                let i = opts.iter().position(|o| *o == s.filter).unwrap_or(0);
                let n = opts.len();
                s.filter = opts[if k == Key::R1 { (i + 1) % n } else { (i + n - 1) % n }];
                self.run_search();
                return Action::None;
            }
            _ => {}
        }
        if s.in_results {
            let n = s.results.len();
            match k {
                Key::Up => s.sel = s.sel.saturating_sub(1),
                Key::Down => s.sel = (s.sel + 1).min(n.saturating_sub(1)),
                Key::Left => s.sel = s.sel.saturating_sub(RESULT_ROWS - 1),
                Key::Right => s.sel = (s.sel + RESULT_ROWS - 1).min(n.saturating_sub(1)),
                Key::A | Key::Start => return self.selected().map_or(Action::None, Action::Launch),
                Key::B => s.in_results = false,
                Key::Y => {
                    if let Some(g) = self.selected() {
                        self.favourite(g);
                    }
                }
                _ => {}
            }
            let s = &mut self.search;
            if s.sel < s.top {
                s.top = s.sel;
            }
            if s.sel >= s.top + RESULT_ROWS {
                s.top = s.sel + 1 - RESULT_ROWS;
            }
            return Action::None;
        }
        let keys = keyboard();
        let (col, row, span, _) = keys[s.key];
        let find = |c: usize, r: usize| keys.iter().position(|&(kc, kr, ks, _)| kr == r && c >= kc && c < kc + ks);
        let rows = keys.iter().map(|k| k.1).max().unwrap_or(0) + 1;
        match k {
            Key::Left => s.key = find((col + KEY_COLS - 1) % KEY_COLS, row).unwrap_or(s.key),
            Key::Right => s.key = find((col + span) % KEY_COLS, row).unwrap_or(s.key),
            Key::Up => s.key = find(col, (row + rows - 1) % rows).unwrap_or(s.key),
            Key::Down => s.key = find(col, (row + 1) % rows).unwrap_or(s.key),
            Key::A => match keys[s.key].3 {
                KeyCap::Char(c) => self.type_char(c),
                KeyCap::Space => self.type_char(' '),
                KeyCap::Delete => self.delete(),
            },
            Key::Y => self.type_char(' '),
            Key::B => {
                if self.search.query.is_empty() {
                    self.screen = Screen::Library;
                } else {
                    self.delete();
                }
            }
            Key::Start if !self.search.results.is_empty() => self.search.in_results = true,
            Key::Select => self.screen = Screen::Library,
            _ => {}
        }
        Action::None
    }

    /// Sets the search query (snapshot scripts).
    pub fn set_query(&mut self, q: &str) {
        self.screen = Screen::Search;
        self.search.query = q.to_string();
        self.run_search();
    }

    fn type_char(&mut self, c: char) {
        if c == ' ' && (self.search.query.is_empty() || self.search.query.ends_with(' ')) {
            return;
        }
        if self.search.query.len() < 40 {
            self.search.query.push(c);
            self.run_search();
        }
    }

    fn delete(&mut self) {
        self.search.query.pop();
        self.run_search();
    }

    fn open_settings(&mut self, section: usize) {
        self.screen = Screen::Settings;
        self.set.section = section;
        self.set.row = rows(section).iter().position(|r| r.selectable()).unwrap_or(0);
        self.set.scroll = 0;
        self.set.confirm_off = None;
        self.card_space = settings::card_space(&self.lib.root);
        if section == SYSTEM_SECTION {
            self.stats = self.sampler.sample();
            self.stats_at = Instant::now();
        }
        self.usb_control = std::fs::read_to_string(self.lib.root.join("gb350/agent.conf"))
            .is_ok_and(|t| t.lines().any(|l| l.replace(' ', "") == "usb_control=on"));
        self.battery = settings::battery();
    }

    fn settings_key(&mut self, k: Key) -> Action {
        let rs = rows(self.set.section);
        let row = rs[self.set.row];
        match k {
            Key::B => {
                self.screen = Screen::Library;
                self.set.confirm_off = None;
            }
            Key::L1 | Key::R1 => {
                let n = SECTIONS.len();
                let s = if k == Key::R1 { (self.set.section + 1) % n } else { (self.set.section + n - 1) % n };
                self.open_settings(s);
            }
            Key::Up | Key::Down => {
                let mut i = self.set.row as isize;
                loop {
                    i += if k == Key::Down { 1 } else { -1 };
                    if i < 0 || i >= rs.len() as isize {
                        break;
                    }
                    if rs[i as usize].selectable() {
                        self.set.row = i as usize;
                        break;
                    }
                }
                self.set.confirm_off = None;
            }
            Key::Left | Key::Right => self.change(row, k == Key::Right),
            Key::A => match row {
                Row::PowerOff => {
                    if self.set.confirm_off.is_some() {
                        return Action::PowerOff;
                    }
                    self.set.confirm_off = Some(Instant::now());
                }
                Row::Rescan => {
                    self.lib = Library::scan(&self.lib.root);
                    self.previews.clear();
                    self.cursors.clear();
                    self.refresh_tabs(None);
                    self.run_search();
                    let n = self.lib.total();
                    self.toast(format!("Found {n} game{}", if n == 1 { "" } else { "s" }));
                }
                Row::ExitConsole => return Action::Exit,
                Row::Storage => self.storage_enter(),
                Row::ScreenOff => return Action::ScreenOff,
                r => self.change(r, true),
            },
            _ => {}
        }
        Action::None
    }

    fn change(&mut self, row: Row, up: bool) {
        if row == Row::UsbControl {
            self.usb_control = !self.usb_control;
            ask_agent(if self.usb_control { "usb-control on" } else { "usb-control off" });
            return;
        }
        let s = &mut self.settings;
        let cycle3 = |i: usize| if up { (i + 1) % 3 } else { (i + 2) % 3 };
        match row {
            Row::Brightness => {
                s.brightness = if up { (s.brightness + 10).min(100) } else { s.brightness.saturating_sub(10).max(10) };
                s.apply_brightness();
            }
            Row::Volume => {
                s.volume = if up { (s.volume + VOLUME_STEP).min(100) } else { s.volume.saturating_sub(VOLUME_STEP) };
                s.apply_volume();
            }
            Row::Scaling => {
                let all = [Scaling::Aspect, Scaling::Integer, Scaling::Stretch];
                s.scaling = all[cycle3(all.iter().position(|&x| x == s.scaling).unwrap_or(0))];
            }
            Row::Filter => s.filter = if s.filter == Filter::Smooth { Filter::Sharp } else { Filter::Smooth },
            Row::Perf => s.perf = !s.perf,
            Row::Vertical => s.sideways = !s.sideways,
            Row::Ps1Cores => s.ps1_two_cores = !s.ps1_two_cores,
            Row::Colour | Row::Contrast | Row::Sharpness | Row::Border => {
                let step = |v: u8, unit: u8, max: u8| if up { (v + unit).min(max) } else { v.saturating_sub(unit) };
                match row {
                    Row::Colour => s.colour = step(s.colour, 10, 100),
                    Row::Contrast => s.contrast = step(s.contrast, 10, 100),
                    Row::Sharpness => s.sharpness = step(s.sharpness, 1, 10),
                    _ => {
                        let all = [Border::Black, Border::Grey, Border::Blue];
                        s.border = all[cycle3(all.iter().position(|&x| x == s.border).unwrap_or(0))];
                    }
                }
                s.apply_picture();
            }
            _ => return,
        }
        s.save();
    }


    fn storage_enter(&mut self) {
        let _ = std::fs::remove_file(STORAGE_STATE);
        self.storage = Some(StorageState { state: String::new(), host: false, was_on: false, leaving: false });
        self.screen = Screen::Storage;
        ask_agent("storage on");
    }

    fn storage_leave(&mut self) {
        match &mut self.storage {
            Some(s) if s.was_on => {
                s.leaving = true;
                ask_agent("storage off");
            }
            // Never handed over (error): just go back.
            _ => self.storage_done(),
        }
    }

    /// Follows the agent's state; true if the screen changed.
    fn poll_storage(&mut self) -> bool {
        let state = std::fs::read_to_string(STORAGE_STATE).map(|s| s.trim().to_string()).unwrap_or_default();
        let host = std::fs::read_dir("/sys/class/udc")
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .and_then(|e| std::fs::read_to_string(e.path().join("state")).ok())
            .is_some_and(|s| s.trim() == "configured");
        let Some(s) = &mut self.storage else { return false };
        let changed = s.state != state || s.host != host;
        s.state = state;
        s.host = host;
        if s.state == "on" {
            s.was_on = true;
        }
        if s.was_on && s.state == "off" {
            self.storage_done();
            return true;
        }
        changed
    }

    /// The card is back: read it again (games may have been added).
    fn storage_done(&mut self) {
        let was_on = self.storage.take().is_some_and(|s| s.was_on);
        if was_on {
            self.lib = Library::scan(&self.lib.root);
            self.previews.clear();
            self.cursors.clear();
            self.refresh_tabs(None);
            self.settings.reload();
            let n = self.lib.total();
            self.toast(format!("SD card is back: {n} game{}", if n == 1 { "" } else { "s" }));
        }
        self.open_settings(2);
    }

    /// After a game: back to the list, which now has it in Recent.
    pub fn after_game(&mut self, g: u32) {
        self.lib.played(g);
        self.save_cursor();
        let keep = self.current_tab();
        self.refresh_tabs(keep);
        if keep == Some(Tab::Recent) {
            self.sel = 0;
            self.top = 0;
        }
        self.settings.reload();
    }
}
