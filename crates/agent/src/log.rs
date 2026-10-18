//! Logging to the SD card (always) and the log screen (`console.rs`).
//!
//! The SD log is the fallback channel: if both the screen and USB fail, the card
//! can be read on the Mac afterwards.
//!
//! To spare the card (each tiny fsync rewrites a whole flash block plus the
//! exFAT metadata), lines are buffered and written + synced every few seconds
//! by a background thread, immediately for errors, and on `close_file`.

use crate::console::{Console, Level};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<File>,
    /// Lines not yet written to `file`.
    pending: Vec<u8>,
    screen: Option<Console>,
    /// Kernel log (PID 1 on mainline: shows on the framebuffer console).
    kmsg: Option<File>,
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger { file: None, pending: Vec::new(), screen: None, kmsg: None });

/// How often buffered lines reach the card.
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

impl Logger {
    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if !self.pending.is_empty() {
                let _ = f.write_all(&self.pending);
                let _ = f.sync_data();
            }
        }
        self.pending.clear();
    }

    fn append(&mut self, line: &str) {
        if self.file.is_some() {
            self.pending.extend_from_slice(line.as_bytes());
            self.pending.push(b'\n');
        }
    }
}

/// Writes buffered lines to the card now.
pub fn flush() {
    LOGGER.lock().unwrap().flush();
}

/// Mirror log lines into the kernel log.
pub fn to_kmsg() {
    LOGGER.lock().unwrap().kmsg = OpenOptions::new().write(true).open("/dev/kmsg").ok();
}

pub fn init_file(path: &str) {
    let file = OpenOptions::new().create(true).append(true).open(path);
    let mut l = LOGGER.lock().unwrap();
    match file {
        Ok(f) => l.file = Some(f),
        Err(e) => eprintln!("cannot open log {path}: {e}"),
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(FLUSH_INTERVAL);
        flush();
    });
}

/// Logs to `path` again after `close_file`.
pub fn reopen_file(path: &str) {
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        LOGGER.lock().unwrap().file = Some(f);
    }
}

/// Writes out pending lines and stops logging to the card (so it can be
/// unmounted).
pub fn close_file() {
    let mut l = LOGGER.lock().unwrap();
    l.flush();
    l.file = None;
}

pub fn set_screen(console: Console) {
    LOGGER.lock().unwrap().screen = Some(console);
}

pub fn with_screen<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    LOGGER.lock().unwrap().screen.as_mut().map(f)
}

pub fn uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

pub fn log(level: Level, msg: &str) {
    let line = format!("[{:8.2}] {msg}", uptime());
    let mut l = LOGGER.lock().unwrap();
    // With the kernel log as a sink (PID 1), stderr is the same console.
    if l.kmsg.is_none() {
        eprintln!("{line}");
    }
    l.append(&line);
    if level == Level::Error {
        l.flush();
    }
    if let Some(k) = l.kmsg.as_mut() {
        // One write() per record: each write to /dev/kmsg is a separate line.
        let _ = k.write_all(format!("<5>agent: {msg}\n").as_bytes());
    }
    if let Some(s) = l.screen.as_mut() {
        s.push_line(msg, level);
    }
}

pub fn info(msg: &str) {
    log(Level::Info, msg);
}

pub fn ok(msg: &str) {
    log(Level::Ok, msg);
}

pub fn warn(msg: &str) {
    log(Level::Warn, msg);
}

pub fn error(msg: &str) {
    log(Level::Error, msg);
}

pub fn remote(msg: &str) {
    log(Level::Remote, msg);
}

/// Shows kernel messages of warning priority or worse on the agent's console
/// (the kernel's own console is hidden while the agent holds the display).
/// Not the agent's own lines, which it mirrors into the kernel log.
pub fn mirror_kernel_warnings() {
    let Ok(mut f) = File::open("/dev/kmsg") else { return };
    // Only what comes from now on.
    unsafe { libc::lseek(std::os::fd::AsRawFd::as_raw_fd(&f), 0, libc::SEEK_END) };
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            // One record per read: "prio,seq,usec,flags;text\n[ KEY=value...]"
            let n = match std::io::Read::read(&mut f, &mut buf) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue, // overrun
                Err(_) => return,
            };
            let rec = String::from_utf8_lossy(&buf[..n]);
            let Some((head, text)) = rec.split_once(';') else { continue };
            let level = head.split(',').next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(6) & 7;
            let text = text.lines().next().unwrap_or("");
            if level > 4 || text.starts_with("agent: ") {
                continue;
            }
            let color = if level <= 3 { Level::Error } else { Level::Warn };
            if let Some(s) = LOGGER.lock().unwrap().screen.as_mut() {
                s.push_line(&format!("kernel: {text}"), color);
            }
        }
    });
}

/// Writes to the SD log only (for verbose detail that would flood the screen).
pub fn file_only(msg: &str) {
    let line = format!("[{:8.2}] {msg}", uptime());
    LOGGER.lock().unwrap().append(&line);
}
