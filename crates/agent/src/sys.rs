//! System helpers: PID 1 duties, device info, power off / reboot and
//! physical memory access.

use crate::{log, usb};
use std::ffi::CStr;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn uname() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return "uname failed".into();
    }
    let f = |s: &[libc::c_char]| unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {} {} {}", f(&u.sysname), f(&u.release), f(&u.version), f(&u.machine))
}

fn meminfo() -> String {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .filter(|l| l.starts_with("MemTotal") || l.starts_with("MemAvailable") || l.starts_with("MemFree"))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn info() -> String {
    let fb = log::with_screen(|s| s.describe()).unwrap_or_else(|| "no console".into());
    let role = usb::role_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "?".into());
    format!(
        "agent {VERSION}\nkernel {}\nuptime {:.1}s\nmem {}\nusb udc {} state {} role {}\nbattery {}, headphones {}, held [{}]\n{fb}\n",
        uname(),
        log::uptime(),
        meminfo(),
        usb::UDC,
        usb::udc_state(),
        role,
        crate::buttons::battery(),
        if crate::buttons::headphones() { "in" } else { "out" },
        crate::buttons::held().join(" "),
    )
}

static BASE: OnceLock<PathBuf> = OnceLock::new();

/// The GB350 directory on the card: logs, kernel modules, binaries (set once at
/// startup).
pub fn set_base(base: PathBuf) {
    let _ = BASE.set(base);
}

pub fn base() -> &'static Path {
    BASE.get().map(PathBuf::as_path).unwrap_or(Path::new("/mnt/sdcard/gb350"))
}

/// Where the full BusyBox's applet links are installed (tmpfs; the SD card is
/// exFAT and cannot hold symlinks).
const TOOL_DIR: &str = "/tmp/bin";
pub const PATH: &str = "/tmp/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// PID 1 duties on the mainline initramfs: the pseudo filesystems.
pub fn init_mounts() {
    for (src, target, fstype) in
        [("devtmpfs", "/dev", "devtmpfs"), ("proc", "/proc", "proc"), ("sysfs", "/sys", "sysfs"), ("tmpfs", "/tmp", "tmpfs")]
    {
        let _ = std::fs::create_dir_all(target);
        let (s, t, f) = (
            std::ffi::CString::new(src).unwrap(),
            std::ffi::CString::new(target).unwrap(),
            std::ffi::CString::new(fstype).unwrap(),
        );
        unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), 0, std::ptr::null()) };
    }
    std::env::set_var("PATH", PATH);
}

/// Mounts the SD card's exFAT partition at /mnt/sdcard (mainline: the MMC
/// driver probes asynchronously, so wait a little for the block device).
/// As PID 1: the kernel's orderly_poweroff() runs `poweroff_cmd` (on the
/// GB350 when the second core asks to shut down: SD card removed, battery
/// critical). Point it at a SIGUSR2 to the agent (busybox init's "power off") and
/// shut down cleanly on that signal. Call before any thread is spawned: every
/// thread must block the signal, or the kernel drops it (PID 1 has no
/// handler) instead of leaving it for `sigwait`. SIGHUP calls `hangup`
/// (restarts the launcher), SIGUSR1 `usr1` (USB storage requests).
pub fn handle_poweroff_signal(power_off: fn() -> !, hangup: fn(), usr1: fn()) {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR2);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    if let Err(e) = std::fs::write("/proc/sys/kernel/poweroff_cmd", "/bin/busybox kill -USR2 1") {
        log::warn(&format!("poweroff_cmd: {e}"));
    }
    std::thread::spawn(move || loop {
        let mut sig = 0;
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            continue;
        }
        if sig == libc::SIGUSR2 {
            log::warn("power: asked to power off");
            power_off();
        }
        if sig == libc::SIGHUP {
            hangup();
        }
        if sig == libc::SIGUSR1 {
            usr1();
        }
    });
}

pub fn mount_sdcard() -> Result<String, String> {
    let dev = "/dev/mmcblk0p1";
    for _ in 0..50 {
        if std::path::Path::new(dev).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !std::path::Path::new(dev).exists() {
        return Err(format!("{dev} did not appear"));
    }
    let _ = std::fs::create_dir_all("/mnt/sdcard");
    let (s, t, f) = (
        std::ffi::CString::new(dev).unwrap(),
        std::ffi::CString::new("/mnt/sdcard").unwrap(),
        std::ffi::CString::new("exfat").unwrap(),
    );
    let rc = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), libc::MS_NOATIME, std::ptr::null()) };
    if rc != 0 {
        return Err(format!("mount {dev}: {}", std::io::Error::last_os_error()));
    }
    Ok(format!("{dev} on /mnt/sdcard"))
}

/// Installs the applet links of the initramfs's full static BusyBox (for
/// `gbctl sh`).
pub fn install_tools() -> Result<usize, String> {
    let bb = Path::new("/bin/busybox");
    std::fs::create_dir_all(TOOL_DIR).map_err(|e| format!("mkdir {TOOL_DIR}: {e}"))?;
    let status = std::process::Command::new(&bb)
        .args(["--install", "-s", TOOL_DIR])
        .status()
        .map_err(|e| format!("run {}: {e}", bb.display()))?;
    if !status.success() {
        return Err(format!("busybox --install: {status}"));
    }
    Ok(std::fs::read_dir(TOOL_DIR).map(|d| d.count()).unwrap_or(0))
}

/// Replaces this process with `path` (same pid; fds are close-on-exec).
pub fn exec_self(path: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(path).arg(base()).exec()
}

/// Loadable modules live on the card (gb350/kmod/lib/modules/<release>,
/// from the kernel build): point /lib/modules there so
/// `modprobe` finds them.
pub fn link_modules(base: &Path) {
    let dir = base.join("kmod/lib/modules");
    if dir.is_dir() {
        let _ = std::fs::create_dir_all("/lib");
        let _ = std::os::unix::fs::symlink(&dir, "/lib/modules");
    }
}

/// Kernel log (dmesg) of this boot, saved to `<base>/kernel-logs/<n>.log`
/// just before a reboot or power-off (the last 10 are kept). Done only then,
/// rather than periodically, to keep writes to the card down.
pub fn save_kernel_log() -> Option<String> {
    const SYSLOG_ACTION_READ_ALL: libc::c_int = 3;
    const SYSLOG_ACTION_SIZE_BUFFER: libc::c_int = 10;
    let size = unsafe { libc::klogctl(SYSLOG_ACTION_SIZE_BUFFER, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe { libc::klogctl(SYSLOG_ACTION_READ_ALL, buf.as_mut_ptr() as *mut libc::c_char, size) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let dir = base().join("kernel-logs");
    let _ = std::fs::create_dir_all(&dir);
    let mut existing: Vec<u32> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".log")?.parse().ok()).collect())
        .unwrap_or_default();
    existing.sort();
    for old in existing.iter().rev().skip(9) {
        let _ = std::fs::remove_file(dir.join(format!("{old}.log")));
    }
    let path = dir.join(format!("{}.log", existing.last().map_or(1, |l| l + 1)));
    Some(match std::fs::write(&path, &buf) {
        Ok(()) => format!("kernel log saved to {}", path.display()),
        Err(e) => format!("kernel log: {}: {e}", path.display()),
    })
}

/// Unmounts the SD card before a restart, so exFAT's volume-dirty flag is
/// cleared (otherwise the next mount warns "not properly unmounted"); falls
/// back to a read-only remount if something still holds a file open.
pub fn release_sdcard() -> String {
    if let Some(msg) = save_kernel_log() {
        log::info(&msg);
    }
    log::close_file();
    unsafe { libc::sync() };
    let path = c"/mnt/sdcard";
    if unsafe { libc::umount(path.as_ptr()) } == 0 {
        return "sdcard unmounted".into();
    }
    let e = std::io::Error::last_os_error();
    let flags = libc::MS_REMOUNT | libc::MS_RDONLY;
    if unsafe { libc::mount(std::ptr::null(), path.as_ptr(), std::ptr::null(), flags, std::ptr::null()) } == 0 {
        return format!("sdcard remounted read-only (umount: {e})");
    }
    format!("sdcard left mounted (umount: {e}; remount ro: {})", std::io::Error::last_os_error())
}

/// PID 1 inherits processes whose parent exited (a `gbctl sh` command's
/// background jobs): reap those that have ended. Called once a second; a
/// zombie is reaped only when seen twice, so the agent's own children (which their
/// threads wait for at once) are never taken from under them.
pub fn reap_orphans() {
    static SEEN: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());
    let mut zombies = vec![];
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // "pid (comm) S ppid ...": comm may contain spaces; parse after ')'.
        let Some(rest) = stat.rsplit_once(')').map(|r| r.1) else { continue };
        let mut f = rest.split_whitespace();
        if f.next() == Some("Z") && f.next() == Some("1") {
            zombies.push(pid);
        }
    }
    let mut seen = SEEN.lock().unwrap();
    for &pid in zombies.iter().filter(|p| seen.contains(p)) {
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    }
    *seen = zombies;
}

/// Is the SD card mounted?
pub fn card_mounted() -> bool {
    std::fs::read_to_string("/proc/mounts").is_ok_and(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/mnt/sdcard")))
}

/// Clean power-off (the second core asked, via the kernel's
/// orderly_poweroff, or the launcher did): logs saved, card released, then
/// the kernel's power-off handler has the second core drop the power latch.
pub fn power_off() -> ! {
    log::warn("power: shutting down");
    log::info(&release_sdcard());
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
    log::error(&format!("power: reboot(POWER_OFF): {}", std::io::Error::last_os_error()));
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

pub fn reboot() -> ! {
    unsafe { libc::sync() };
    std::thread::sleep(std::time::Duration::from_millis(300));
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Reads `len` bytes at physical `addr` (as aligned 32-bit loads), or performs
/// a single store of `(width, value)`, via `/dev/mem`.
///
/// Runs in a forked child: touching an address with nothing behind it raises a
/// bus error, which then kills the child instead of the agent.
pub fn mem_op(addr: u32, len: u32, store: Option<(u8, u32)>) -> Result<Vec<u8>, String> {
    const PAGE: u32 = 4096;
    let base = addr & !(PAGE - 1);
    let off = (addr - base) as usize;
    let map_len = (off + len as usize).div_ceil(PAGE as usize) * PAGE as usize;

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child: only raw libc calls from here on.
        unsafe {
            libc::close(fds[0]);
            let fd = libc::open(c"/dev/mem".as_ptr(), libc::O_RDWR | libc::O_SYNC);
            if fd < 0 {
                libc::_exit(2);
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                base as libc::off_t,
            );
            if p == libc::MAP_FAILED {
                libc::_exit(3);
            }
            let ptr = (p as *mut u8).add(off);
            match store {
                Some((1, v)) => std::ptr::write_volatile(ptr, v as u8),
                Some((2, v)) => std::ptr::write_volatile(ptr as *mut u16, v as u16),
                Some((_, v)) => std::ptr::write_volatile(ptr as *mut u32, v),
                None => {
                    let mut chunk = [0u8; 4096];
                    let mut done = 0usize;
                    while done < len as usize {
                        let n = (len as usize - done).min(chunk.len());
                        for w in (0..n).step_by(4) {
                            let v = std::ptr::read_volatile(ptr.add(done + w) as *const u32);
                            chunk[w..w + 4].copy_from_slice(&v.to_le_bytes());
                        }
                        let mut sent = 0;
                        while sent < n {
                            let r = libc::write(fds[1], chunk[sent..n].as_ptr() as *const _, n - sent);
                            if r <= 0 {
                                libc::_exit(4);
                            }
                            sent += r as usize;
                        }
                        done += n;
                    }
                }
            }
            libc::_exit(0);
        }
    }

    unsafe { libc::close(fds[1]) };
    let mut data = Vec::new();
    let read_result = unsafe { std::fs::File::from_raw_fd(fds[0]) }.read_to_end(&mut data);

    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        return Err(format!(
            "access at {addr:#010x} killed the worker with signal {sig}{}",
            if sig == libc::SIGBUS { " (bus error: nothing at that address?)" } else { "" }
        ));
    }
    match libc::WEXITSTATUS(status) {
        0 => {}
        2 => return Err("open /dev/mem failed".into()),
        3 => return Err(format!("mmap /dev/mem at {base:#010x} failed")),
        n => return Err(format!("memory worker exited with {n}")),
    }
    read_result.map_err(|e| format!("pipe read: {e}"))?;
    if store.is_none() && data.len() != len as usize {
        return Err(format!("short read: {} of {len} bytes", data.len()));
    }
    Ok(data)
}
