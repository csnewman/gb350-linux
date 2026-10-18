//! What the panel shows when the launcher can't run (missing, crashing, no
//! SD card): what went wrong and what can be done - try again, USB storage
//! (to copy the launcher back from a computer), the log, power off. Drawn in
//! the launcher's look (`gb350_ui::notice`); buttons from the kernel's input
//! devices.

use crate::console::Hints;
use crate::{launcher, log, storage, sys};
use gb350_ui::display::Display;
use gb350_ui::input::{Event, Input, Key};
use gb350_ui::notice::{self, Icon, Notice};
use gb350_ui::Ui;
use std::time::Duration;

pub enum Problem {
    /// No launcher binary on the card.
    Missing,
    /// It failed this many times in a row; the last way it ended.
    Crashing(String),
    /// The card did not mount (why).
    NoCard(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Retry,
    Storage,
    Log,
    PowerOff,
}

/// Shows the problem until the user picks "Try again" (or something else
/// makes trying again worthwhile: the card came back from USB storage, a
/// `kill -HUP 1`). Returns then; powers off if asked.
pub fn run(problem: &Problem) {
    log::with_screen(|s| s.release());
    let mut ui = Ui::new();
    let mut input = Input::open();
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let base_footer = format!("agent {} · Linux {}", sys::VERSION, release.trim());
    let (title, body, detail, acts): (&str, &str, String, &[Act]) = match problem {
        Problem::Missing => (
            "The launcher is missing",
            "gb350/bin/launcher isn't on the SD card. Connect the console to a computer and copy it back, or set the card up again.",
            String::new(),
            &[Act::Retry, Act::Storage, Act::Log, Act::PowerOff],
        ),
        Problem::Crashing(how) => (
            "The launcher keeps crashing",
            "It stopped 5 times in a minute. Its last messages are in gb350/agent.log on the SD card.",
            format!("last exit: {how} · "),
            &[Act::Retry, Act::Storage, Act::Log, Act::PowerOff],
        ),
        Problem::NoCard(why) => (
            "No SD card",
            "The games and the launcher live on the SD card. Power off, insert the card and power on again.",
            format!("{why} · "),
            &[Act::PowerOff, Act::Log],
        ),
    };
    let footer = format!("{detail}{base_footer}");
    let labels: Vec<&str> = acts
        .iter()
        .map(|a| match a {
            Act::Retry => "Try again",
            Act::Storage => "Connect to computer",
            Act::Log => "Show log",
            Act::PowerOff => "Power off",
        })
        .collect();
    let mut sel = 0;
    loop {
        let Some(mut display) = open_display() else {
            // No display of its own: wait for a kick.
            launcher::wait_kick();
            return;
        };
        let n = Notice { icon: Icon::Warning, kicker: "CAN'T START", title, body, actions: &labels, selected: sel, footer: &footer };
        draw(&mut display, &mut ui, &n);
        input.resync();
        let act = loop {
            match next_key(&mut input, Duration::from_millis(500)) {
                Some(Key::Up) => sel = (sel + labels.len() - 1) % labels.len(),
                Some(Key::Down) => sel = (sel + 1) % labels.len(),
                Some(Key::A) => break acts[sel],
                _ => {
                    if launcher::take_kick() {
                        return;
                    }
                    continue;
                }
            }
            let n = Notice { selected: sel, ..n };
            draw(&mut display, &mut ui, &n);
        };
        match act {
            Act::Retry => return,
            Act::PowerOff => sys::power_off(),
            Act::Storage => {
                usb_storage(&mut display, &mut ui, &mut input);
                return;
            }
            Act::Log => {
                drop(display);
                if show_log(true) == LogExit::Launcher {
                    return;
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum LogExit {
    /// B: back to where it was opened from.
    Back,
    /// START, or a kick: start the launcher again.
    Launcher,
}

/// The log on screen, scrollable, until B (with `back`), START or a kick.
pub fn show_log(back: bool) -> LogExit {
    let shown = log::with_screen(|s| {
        s.set_hints(if back { Hints::Back } else { Hints::Launcher });
        s.take()
    });
    if !matches!(shown, Some(Ok(()))) {
        log::warn(&format!("log screen: {:?}", shown.map(|r| r.err())));
        launcher::wait_kick();
        return LogExit::Launcher;
    }
    let mut input = Input::open();
    input.resync();
    let exit = loop {
        let page = log::with_screen(|s| s.page()).unwrap_or(10);
        let rows = match next_key(&mut input, Duration::from_millis(500)) {
            Some(Key::Up) => 1,
            Some(Key::Down) => -1,
            Some(Key::Left | Key::L1 | Key::L2) => page,
            Some(Key::Right | Key::R1 | Key::R2) => -page,
            Some(Key::B) if back => break LogExit::Back,
            Some(Key::Start) => break LogExit::Launcher,
            _ => {
                if launcher::take_kick() {
                    break LogExit::Launcher;
                }
                continue;
            }
        };
        log::with_screen(|s| s.scroll(rows));
    };
    log::with_screen(|s| s.release());
    exit
}

/// The card as a USB drive until the computer ejects it (or B / A here).
fn usb_storage(display: &mut Display, ui: &mut Ui, input: &mut Input) {
    let body_wait = "Connect a USB-C cable to the computer: the SD card shows up there as a USB drive.";
    let body_on = "The SD card shows up as a USB drive. Copy the launcher to gb350/bin/, then eject the drive on the computer.";
    let mut shown = String::new();
    let mut draw_state = |display: &mut Display, ui: &mut Ui, title: &str, body: &str| {
        let key = format!("{title}{body}");
        if key != shown {
            let n = Notice { icon: Icon::Usb, kicker: "USB STORAGE", title, body, actions: &["Done"], selected: 0, footer: "" };
            draw(display, ui, &n);
            shown = key;
        }
    };
    draw_state(display, ui, "Preparing the SD card…", "");
    storage::enter();
    if !storage::is_on() {
        let err = std::fs::read_to_string(storage::STATE).unwrap_or_default();
        draw_state(display, ui, "Could not share the SD card", err.trim().trim_start_matches("error "));
        while !matches!(next_key(input, Duration::from_secs(60)), Some(Key::B | Key::A)) {}
        return;
    }
    input.resync();
    loop {
        let host = std::fs::read_to_string("/sys/class/udc/musb-hdrc.0.auto/state").is_ok_and(|s| s.trim() == "configured");
        if host {
            draw_state(display, ui, "Connected to your computer", body_on);
        } else {
            draw_state(display, ui, "Connect to a computer", body_wait);
        }
        if !storage::is_on() {
            return; // ejected by the computer: the card is back
        }
        if matches!(next_key(input, Duration::from_millis(500)), Some(Key::B | Key::A)) {
            draw_state(display, ui, "Getting the SD card back…", "");
            storage::leave();
            return;
        }
    }
}

fn open_display() -> Option<Display> {
    match Display::open() {
        Ok(d) => Some(d),
        Err(e) => {
            log::warn(&format!("rescue: display: {e}"));
            None
        }
    }
}

fn draw(display: &mut Display, ui: &mut Ui, n: &Notice) {
    display.settle();
    display.draw(ui, |c| notice::draw(c, n));
    display.settle();
}

/// The next button press within `timeout` (repeats included).
fn next_key(input: &mut Input, timeout: Duration) -> Option<Key> {
    let deadline = std::time::Instant::now() + timeout;
    let mut events = vec![];
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        let wait = input.deadline().map_or(deadline, |d| d.min(deadline));
        let mut fds: Vec<libc::pollfd> = input.fds().into_iter().map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, wait.saturating_duration_since(now).as_millis() as i32) };
        input.read(&mut events);
        input.tick(std::time::Instant::now(), &mut events);
        for e in events.drain(..) {
            if let Event::Press(k) = e {
                return Some(k);
            }
        }
    }
}
