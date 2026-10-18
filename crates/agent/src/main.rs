//! GB350 on-device agent: PID 1 of the GB350 kernel's initramfs.
//!
//! Sets the system up (pseudo filesystems, SD card, kernel modules, tools),
//! shows a text console on the panel until the game launcher takes the
//! display, runs the launcher (restarting it if it crashes), serves the USB
//! control channel (`gbctl`) on the USB-C port, powers off cleanly when asked
//! and switches the SD card to USB storage mode and back. Logs to the card.
//!
//! Usage: `agent [BASE_DIR]` (default `/mnt/sdcard/gb350`): BASE_DIR holds
//! `bin/` (launcher, emulators), `kmod/` (kernel modules) and `agent.log`.

mod audio;
mod buttons;
mod config;
mod console;
mod kexec;
mod launcher;
mod log;
mod rescue;
mod rngseed;
mod server;
mod storage;
mod sys;
mod usb;

use std::path::PathBuf;
use std::time::Duration;

fn main() {
    if std::process::id() != 1 {
        eprintln!("agent: runs as PID 1 (the initramfs's /init)");
        std::process::exit(2);
    }
    // Built with panic = "abort": a panic in any thread would end
    // PID 1 and so panic the kernel. Log it and stop only that thread.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("agent: PANIC (thread {:?}): {info}", std::thread::current().name());
        if let Ok(mut k) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
            use std::io::Write;
            let _ = writeln!(k, "<2>{msg}");
        }
        loop {
            std::thread::park();
        }
    }));
    sys::init_mounts();
    // Before any thread exists (see there).
    sys::handle_poweroff_signal(sys::power_off, launcher::kick, request);
    log::to_kmsg();
    let sd = sys::mount_sdcard();
    let default_base = if sd.is_ok() {
        "/mnt/sdcard/gb350"
    } else {
        let _ = std::fs::create_dir_all("/tmp/gb350");
        "/tmp/gb350"
    };
    let base = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| default_base.into()));
    sys::set_base(base.clone());
    log::init_file(&base.join("agent.log").to_string_lossy());
    log::file_only(&format!("==== gb350-agent {} starting ====", sys::VERSION));
    match &sd {
        Ok(m) => log::info(&format!("sdcard: {m}")),
        Err(e) => log::warn(&format!("sdcard: {e}")),
    }
    sys::link_modules(&base);
    if sd.is_ok() {
        // Never boot twice with the same seed (cold boots read the card's).
        if let Err(e) = rngseed::refresh_file(kexec::DTB) {
            log::warn(&format!("rng-seed: {e}"));
        }
    }

    // The log screen, mirroring kernel warnings. Not shown at boot - the
    // boot logo stays until the launcher draws - only for "Show log" and
    // "Exit to console".
    log::set_screen(console::Console::new("Log"));
    log::mirror_kernel_warnings();
    log::info(&sys::uname());
    log::info(&format!("base dir {}", base.display()));

    match sys::install_tools() {
        Ok(n) => log::info(&format!("tools: {n} busybox applets in /tmp/bin")),
        Err(e) => log::warn(&format!("tools: {e}")),
    }
    let control = config::usb_control();
    match usb::setup(control) {
        Ok(()) => log::ok(&format!("usb: ready (control channel {})", if control { "on" } else { "off" })),
        Err(e) => log::error(&format!("usb: {e}")),
    }

    // Housekeeping once a second: the console's status line (while it is on
    // screen) and reaping orphans.
    std::thread::spawn(|| loop {
        sys::reap_orphans();
        if log::with_screen(|s| s.showing()).unwrap_or(false) {
            let status = format!("agent {} · USB {} · {} · up {:.0} s", sys::VERSION, usb::udc_state(), buttons::battery(), log::uptime());
            log::with_screen(|s| s.set_status(&status));
        }
        std::thread::sleep(Duration::from_secs(1));
    });

    launcher::start(base);
    server::run();
}

/// Requests from the launcher: a line in `/tmp/gb350-agent.request`, then
/// SIGUSR1 to the agent. `storage on|off`, `usb-control on|off`.
pub const REQUEST: &str = "/tmp/gb350-agent.request";

fn request() {
    let text = std::fs::read_to_string(REQUEST).unwrap_or_default();
    let (what, arg) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
    match what {
        "storage" => storage::request(arg),
        "usb-control" => {
            let on = arg == "on";
            if let Err(e) = config::set_usb_control(on) {
                log::warn(&format!("usb control: {e}"));
            }
            std::thread::spawn(move || {
                if let Err(e) = usb::set_control(on) {
                    log::warn(&format!("usb control: {e}"));
                }
            });
        }
        other => log::warn(&format!("request: unknown {other:?}")),
    }
}
