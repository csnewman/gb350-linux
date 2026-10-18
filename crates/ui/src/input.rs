//! Buttons from evdev (`gb350-buttons`), with key repeat for navigation.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    X,
    Y,
    L1,
    R1,
    L2,
    R2,
    Select,
    Start,
    VolUp,
    VolDown,
    Power,
}

pub enum Event {
    /// A press, or a repeat of a held navigation key.
    Press(Key),
    Headphones(bool),
}

/// Key codes as the device tree maps them (labels as printed on the case:
/// A is BTN_EAST, B is BTN_SOUTH).
fn key(code: u16) -> Option<Key> {
    Some(match code {
        0x220 => Key::Up,
        0x221 => Key::Down,
        0x222 => Key::Left,
        0x223 => Key::Right,
        0x131 => Key::A,
        0x130 => Key::B,
        0x133 => Key::X,
        0x134 => Key::Y,
        0x136 => Key::L1,
        0x137 => Key::R1,
        0x138 => Key::L2,
        0x139 => Key::R2,
        0x13a => Key::Select,
        0x13b => Key::Start,
        115 => Key::VolUp,
        114 => Key::VolDown,
        116 => Key::Power,
        _ => return None,
    })
}

impl Key {
    /// For scripts (snapshot mode): `up`, `a`, `l2`, `start`, ...
    pub fn parse(s: &str) -> Option<Key> {
        Some(match s {
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "a" => Key::A,
            "b" => Key::B,
            "x" => Key::X,
            "y" => Key::Y,
            "l1" | "l" => Key::L1,
            "r1" | "r" => Key::R1,
            "l2" => Key::L2,
            "r2" => Key::R2,
            "select" => Key::Select,
            "start" => Key::Start,
            "volup" => Key::VolUp,
            "voldown" => Key::VolDown,
            "power" => Key::Power,
            _ => return None,
        })
    }
}

fn repeats(k: Key) -> bool {
    matches!(k, Key::Up | Key::Down | Key::Left | Key::Right | Key::L2 | Key::R2 | Key::VolUp | Key::VolDown)
}

const EV_KEY: u16 = 1;
const EV_SW: u16 = 5;
const SW_HEADPHONE_INSERT: u16 = 2;
/// EVIOCGKEY(96) / EVIOCGSW(8), MIPS ioctl encoding (read = 2 << 29).
const EVIOCGKEY: u32 = (2 << 29) | (96 << 16) | (0x45 << 8) | 0x18;
const EVIOCGSW: u32 = (2 << 29) | (8 << 16) | (0x45 << 8) | 0x1b;

const REPEAT_DELAY: Duration = Duration::from_millis(320);
const REPEAT_RATE: Duration = Duration::from_millis(70);
const REPEAT_FAST: Duration = Duration::from_millis(35);

pub struct Input {
    files: Vec<File>,
    /// Keys held since before a game ended: ignored until released.
    ignored: Vec<u16>,
    repeat: Option<(Key, Instant, Instant)>,
    pub headphones: bool,
}

impl Input {
    pub fn open() -> Input {
        let mut files = vec![];
        for e in std::fs::read_dir("/dev/input").into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with("event") {
                if let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(e.path()) {
                    files.push(f);
                }
            }
        }
        let mut input = Input { files, ignored: vec![], repeat: None, headphones: false };
        input.headphones = input.switch(SW_HEADPHONE_INSERT);
        input
    }

    pub fn fds(&self) -> Vec<RawFd> {
        self.files.iter().map(|f| f.as_raw_fd()).collect()
    }

    /// Headphones plugged in right now (asks the device).
    pub fn headphones_now(&self) -> bool {
        self.switch(SW_HEADPHONE_INSERT)
    }

    fn switch(&self, sw: u16) -> bool {
        self.files.iter().any(|f| {
            let mut bits = [0u8; 8];
            (unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGSW as _, bits.as_mut_ptr()) }) >= 0 && bits[sw as usize / 8] >> (sw % 8) & 1 != 0
        })
    }

    /// Reads all pending events.
    pub fn read(&mut self, out: &mut Vec<Event>) {
        let mut buf = [0u8; 16 * 32];
        for i in 0..self.files.len() {
            while let Ok(n) = self.files[i].read(&mut buf) {
                if n == 0 {
                    break;
                }
                for e in buf[..n].chunks_exact(16) {
                    let (ty, code) = (u16::from_ne_bytes([e[8], e[9]]), u16::from_ne_bytes([e[10], e[11]]));
                    let value = i32::from_ne_bytes([e[12], e[13], e[14], e[15]]);
                    self.event(ty, code, value, out);
                }
            }
        }
    }

    fn event(&mut self, ty: u16, code: u16, value: i32, out: &mut Vec<Event>) {
        if ty == EV_SW && code == SW_HEADPHONE_INSERT {
            self.headphones = value != 0;
            out.push(Event::Headphones(self.headphones));
            return;
        }
        if ty != EV_KEY || value == 2 {
            return;
        }
        if let Some(i) = self.ignored.iter().position(|&c| c == code) {
            if value == 0 {
                self.ignored.swap_remove(i);
            }
            return;
        }
        let Some(k) = key(code) else { return };
        if value != 0 {
            out.push(Event::Press(k));
            if repeats(k) {
                let now = Instant::now();
                self.repeat = Some((k, now, now + REPEAT_DELAY));
            }
        } else if self.repeat.is_some_and(|(r, _, _)| r == k) {
            self.repeat = None;
        }
    }

    /// When the next repeat is due.
    pub fn deadline(&self) -> Option<Instant> {
        self.repeat.map(|(_, _, next)| next)
    }

    /// Emits due repeats.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Event>) {
        if let Some((k, since, next)) = &mut self.repeat {
            if now >= *next {
                out.push(Event::Press(*k));
                let rate = if now - *since > Duration::from_millis(1500) { REPEAT_FAST } else { REPEAT_RATE };
                *next = now + rate;
            }
        }
    }

    /// After another program had the buttons: drops queued events and
    /// ignores keys still held (e.g. Select + Start) until released.
    pub fn resync(&mut self) {
        let mut buf = [0u8; 16 * 32];
        for f in &mut self.files {
            while matches!(f.read(&mut buf), Ok(n) if n > 0) {}
        }
        self.repeat = None;
        self.ignored.clear();
        for f in &self.files {
            let mut bits = [0u8; 96];
            if unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGKEY as _, bits.as_mut_ptr()) } < 0 {
                continue;
            }
            for code in 0..(96 * 8) as u16 {
                if bits[code as usize / 8] >> (code % 8) & 1 != 0 {
                    self.ignored.push(code);
                }
            }
        }
        self.headphones = self.switch(SW_HEADPHONE_INSERT);
    }
}
