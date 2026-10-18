//! USB storage mode: the SD card becomes a USB drive on the computer (the
//! mass-storage function of the agent's gadget, present from boot with no medium).
//! The card cannot be mounted here at the same time (two writers would
//! corrupt exFAT), so entering unmounts it and leaving - the computer ejects
//! the drive, or the launcher asks - mounts it again.
//!
//! The launcher asks with `storage on` / `storage off` (see `main.rs`
//! requests); the agent reports "off", "on" or "error <text>" in [`STATE`]. The
//! agent's own error screen uses [`enter`] / [`leave`] directly.

use crate::{log, sys};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const STATE: &str = "/tmp/gb350-storage.state";
pub const LUN: &str = "/sys/kernel/config/usb_gadget/gb350/functions/mass_storage.0/lun.0";
const PARTITION: &str = "/dev/mmcblk0p1";
const MOUNT: &str = "/mnt/sdcard";

static ON: AtomicBool = AtomicBool::new(false);
static BUSY: Mutex<()> = Mutex::new(());

fn state(s: &str) {
    let _ = std::fs::write(STATE, s);
}

/// A request from the launcher (`on` / `off`), carried out in the
/// background.
pub fn request(want: &str) {
    let want = want.to_string();
    std::thread::spawn(move || match want.as_str() {
        "on" => enter(),
        "off" => leave(),
        other => log::warn(&format!("storage: unknown request {other:?}")),
    });
}

pub fn is_on() -> bool {
    ON.load(Ordering::SeqCst)
}

/// Hands the card to the computer (blocks while unmounting).
pub fn enter() {
    let _g = BUSY.lock().unwrap();
    enter_locked();
}

/// Takes the card back.
pub fn leave() {
    let _g = BUSY.lock().unwrap();
    leave_locked("asked");
}

fn enter_locked() {
    if ON.load(Ordering::SeqCst) {
        return state("on");
    }
    if !Path::new(LUN).exists() {
        return state("error USB storage is not available");
    }
    log::info("storage: handing the SD card to the computer");
    // Nothing of the agent's may hold the card open.
    log::close_file();
    unsafe { libc::sync() };
    let mnt = c"/mnt/sdcard";
    let mut err = None;
    for _ in 0..10 {
        if unsafe { libc::umount(mnt.as_ptr()) } == 0 {
            err = None;
            break;
        }
        err = Some(std::io::Error::last_os_error());
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(e) = err {
        reopen_log();
        log::warn(&format!("storage: cannot unmount the card: {e}"));
        return state(&format!("error the SD card is in use ({e})"));
    }
    if let Err(e) = std::fs::write(format!("{LUN}/file"), PARTITION) {
        let m = sys::mount_sdcard();
        reopen_log();
        log::warn(&format!("storage: {e} (remount: {m:?})"));
        return state(&format!("error {e}"));
    }
    ON.store(true, Ordering::SeqCst);
    state("on");
    // The computer ejecting the drive empties the LUN: take the card back.
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(500));
        if !ON.load(Ordering::SeqCst) {
            return;
        }
        let file = std::fs::read_to_string(format!("{LUN}/file")).unwrap_or_default();
        if file.trim().is_empty() {
            let _g = BUSY.lock().unwrap();
            leave_locked("ejected by the computer");
            return;
        }
    });
}

fn leave_locked(why: &str) {
    if !ON.load(Ordering::SeqCst) {
        return state("off");
    }
    // forced_eject works even while the computer has the medium locked.
    if std::fs::write(format!("{LUN}/forced_eject"), "1").is_err() {
        let _ = std::fs::write(format!("{LUN}/file"), "");
    }
    ON.store(false, Ordering::SeqCst);
    let m = sys::mount_sdcard();
    reopen_log();
    match m {
        Ok(_) => {
            log::info(&format!("storage: SD card back ({why})"));
            state("off");
        }
        Err(e) => {
            log::warn(&format!("storage: remount: {e}"));
            state(&format!("error the SD card did not mount again: {e}"));
        }
    }
}

fn reopen_log() {
    if Path::new(MOUNT).join("gb350").exists() {
        log::reopen_file(&sys::base().join("agent.log").to_string_lossy());
    }
}
