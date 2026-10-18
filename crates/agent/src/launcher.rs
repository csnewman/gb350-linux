//! Runs the game launcher (`<base>/bin/launcher`) on the panel: hands it the
//! display and restarts it if it crashes. When it can't run - missing,
//! crashing 5 times in a minute, no SD card - the error screen
//! (`rescue.rs`) says so and offers ways out. "Exit to console" (it exits
//! normally) shows the agent's text console until `kill -HUP 1`; so does a
//! `<base>/nolauncher` file (development). It runs from a copy in /tmp and
//! logs into the agent's log, so it holds nothing open on the SD card.

use crate::rescue::{self, Problem};
use crate::{log, sys};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

static KICK: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// (Re)starts the launcher if it is not running (SIGHUP to PID 1).
pub fn kick() {
    *KICK.0.lock().unwrap() = true;
    KICK.1.notify_all();
}

pub fn wait_kick() {
    let mut k = KICK.0.lock().unwrap();
    while !*k {
        k = KICK.1.wait(k).unwrap();
    }
    *k = false;
}

/// A kick arrived (and is consumed).
pub fn take_kick() -> bool {
    std::mem::take(&mut *KICK.0.lock().unwrap())
}

pub fn start(base: PathBuf) {
    std::thread::Builder::new().name("launcher".into()).spawn(move || supervise(base)).expect("launcher thread");
}

fn supervise(base: PathBuf) {
    let mut failures: Vec<Instant> = vec![];
    loop {
        if !sys::card_mounted() {
            rescue::run(&Problem::NoCard("the SD card is not mounted".into()));
            continue;
        }
        let bin = base.join("bin/launcher");
        if base.join("nolauncher").exists() {
            log::info("launcher: not started (nolauncher set); START or `kill -HUP 1` checks again");
            rescue::show_log(false);
            continue;
        }
        if !bin.exists() {
            log::warn(&format!("launcher: {} missing", bin.display()));
            rescue::run(&Problem::Missing);
            continue;
        }
        let root = base.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("/mnt/sdcard"));
        if let Some(Err(e)) = log::with_screen(|s| s.release()) {
            log::warn(&format!("launcher: release display: {e}"));
        }
        // Drop a kick that arrived while it was starting anyway.
        take_kick();
        // Run a copy in RAM: the card must be free to unmount (USB storage).
        let run = PathBuf::from("/tmp/launcher");
        let bin = match std::fs::copy(&bin, &run) {
            Ok(_) => run,
            Err(e) => {
                log::warn(&format!("launcher: copy to /tmp: {e}; running from the card"));
                bin
            }
        };
        // Its messages go to the agent's log (no file of its own on the card, which
        // must be free to unmount for USB storage).
        let status = Command::new(&bin).arg(&root).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().and_then(|mut c| {
            log::file_only(&format!("launcher: started (pid {})", c.id()));
            if let Some(err) = c.stderr.take() {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                        log::file_only(&line);
                    }
                });
            }
            c.wait()
        });
        match status {
            Ok(s) if s.success() => {
                log::info("launcher: exited; START or `kill -HUP 1` starts it again");
                rescue::show_log(false);
            }
            other => {
                let how = match &other {
                    Ok(s) => describe(s),
                    Err(e) => format!("could not start: {e}"),
                };
                log::warn(&format!("launcher: ended: {how}"));
                let now = Instant::now();
                failures.retain(|t| now - *t < Duration::from_secs(60));
                failures.push(now);
                if failures.len() >= 5 {
                    failures.clear();
                    rescue::run(&Problem::Crashing(how));
                } else {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}

/// "exit status 1", "signal 11 (segmentation fault)".
fn describe(s: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(sig)) => {
            let name = match sig {
                4 => " (illegal instruction)",
                6 => " (abort)",
                7 | 10 => " (bus error)",
                9 => " (killed)",
                11 => " (segmentation fault)",
                15 => " (terminated)",
                _ => "",
            };
            format!("signal {sig}{name}")
        }
        _ => "ended".into(),
    }
}
