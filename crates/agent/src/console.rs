//! The agent's log on the panel (`gb350_ui::logview`): what the agent and the kernel
//! reported, for "Show log" on the error screen and "Exit to console". It
//! keeps its lines all the time and only holds the display (DRM master)
//! while shown.

use gb350_ui::display::Display;
use gb350_ui::logview::{self, Entry, LogView};
use gb350_ui::style::Badge;
use gb350_ui::Ui;
use std::collections::VecDeque;

pub use gb350_ui::logview::Level;

/// Lines kept for scrolling back.
const KEEP: usize = 1000;

/// What the button bar offers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hints {
    /// B goes back (to the error screen).
    Back,
    /// START starts the launcher again.
    Launcher,
}

pub struct Console {
    ui: Ui,
    display: Option<Display>,
    entries: VecDeque<Entry>,
    title: String,
    status: String,
    scroll: usize,
    max_scroll: usize,
    hints: Hints,
}

impl Console {
    pub fn new(title: &str) -> Console {
        Console {
            ui: Ui::new(),
            display: None,
            entries: VecDeque::new(),
            title: title.to_string(),
            status: String::new(),
            scroll: 0,
            max_scroll: 0,
            hints: Hints::Launcher,
        }
    }

    /// Takes the display and shows the log (newest at the bottom).
    pub fn take(&mut self) -> Result<(), String> {
        if self.display.is_none() {
            self.display = Some(Display::open()?);
        }
        self.scroll = 0;
        self.redraw();
        Ok(())
    }

    /// Gives the display up.
    pub fn release(&mut self) -> Result<(), String> {
        if let Some(mut d) = self.display.take() {
            d.settle();
        }
        Ok(())
    }

    pub fn showing(&self) -> bool {
        self.display.is_some()
    }

    pub fn set_hints(&mut self, hints: Hints) {
        self.hints = hints;
        self.redraw();
    }

    /// Scrolls `rows` back (negative: forward).
    pub fn scroll(&mut self, rows: isize) {
        self.scroll = (self.scroll as isize + rows).clamp(0, self.max_scroll as isize) as usize;
        self.redraw();
    }

    pub fn page(&self) -> isize {
        self.display.as_ref().map_or(10, |d| logview::rows(d.h) as isize - 1)
    }

    pub fn describe(&self) -> String {
        match &self.display {
            Some(d) => format!("log on screen ({}x{}), {} lines", d.w, d.h, self.entries.len()),
            None => format!("log not shown, {} lines", self.entries.len()),
        }
    }

    pub fn set_status(&mut self, status: &str) {
        if self.status != status {
            self.status = status.to_string();
            self.redraw();
        }
    }

    pub fn push_line(&mut self, text: &str, level: Level) {
        let time = std::fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
        for line in text.lines() {
            self.entries.push_back(Entry { time, text: line.to_string(), level });
        }
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
        self.redraw();
    }

    pub fn redraw(&mut self) {
        let Some(display) = self.display.as_mut() else { return };
        display.settle();
        let hints: &[(Badge, &str)] = match self.hints {
            Hints::Back => &[(Badge::Button("B", false), "Back"), (Badge::UpDown, "Scroll")],
            Hints::Launcher => &[(Badge::Pill("START"), "Start the launcher"), (Badge::UpDown, "Scroll")],
        };
        let slice = self.entries.make_contiguous();
        let view = LogView { title: &self.title, status: &self.status, entries: slice, scroll: self.scroll, hints };
        let mut max = 0;
        display.draw(&mut self.ui, |c| max = logview::draw(c, &view));
        self.max_scroll = max;
    }
}
