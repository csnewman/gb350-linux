//! Exposes the USB-C port as a ConfigFS gadget: the GB350 protocol (see
//! `gb350-proto`) over a vendor-class (0xff) interface with one bulk IN and
//! one bulk OUT endpoint, so no host OS driver claims it (FunctionFS: the
//! agent writes the interface descriptors itself and read/write the endpoint
//! files), plus a mass-storage function for USB storage mode
//! (`storage.rs`). The DT puts the port in peripheral mode.

use crate::log;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const UDC: &str = "musb-hdrc.0.auto";
const FFS_DIR: &str = "/dev/ffs-gb350";
const FFS_FUNCTION: &str = "ffs.gb350";
const ROLE_PATH: &str = "/sys/devices/platform/soc/18844000.usb/musb-hdrc.0.auto/mode";
const CONFIGFS: &str = "/sys/kernel/config";
const GADGET: &str = "/sys/kernel/config/usb_gadget/gb350";

/// The control channel is part of the gadget (agent.conf `usb_control`).
static CONTROL: AtomicBool = AtomicBool::new(false);

pub fn control_on() -> bool {
    CONTROL.load(Ordering::Relaxed)
}

/// The UDC: the vendor name if present, else the first one listed.
fn udc_name() -> String {
    if Path::new("/sys/class/udc").join(UDC).exists() {
        return UDC.into();
    }
    fs::read_dir("/sys/class/udc")
        .ok()
        .and_then(|d| d.flatten().next())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .unwrap_or_else(|| UDC.into())
}

pub fn udc_state() -> String {
    fs::read_to_string(format!("/sys/class/udc/{}/state", udc_name()))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "no-udc".into())
}

pub fn role_path() -> Option<PathBuf> {
    let p = Path::new(ROLE_PATH);
    if p.exists() {
        return Some(p.to_path_buf());
    }
    find_file(Path::new("/sys/devices/platform"), "musb-hdrc.0", "mode", 4)
}

fn find_file(dir: &Path, dir_prefix: &str, name: &str, depth: u32) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let path = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let fname = e.file_name().to_string_lossy().to_string();
        if fname.starts_with(dir_prefix) && path.join(name).exists() {
            return Some(path.join(name));
        }
        if depth > 0 {
            if let Some(found) = find_file(&path, dir_prefix, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn write(path: impl AsRef<Path>, value: &str) -> Result<(), String> {
    let path = path.as_ref();
    fs::write(path, value).map_err(|e| format!("write {} <- {value:?}: {e}", path.display()))
}

fn mkdir(path: impl AsRef<Path>) -> Result<(), String> {
    let path = path.as_ref();
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("mkdir {}: {e}", path.display())),
    }
}

fn configfs_mounted() -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(CONFIGFS)))
        .unwrap_or(false)
}

fn mount_configfs() -> Result<(), String> {
    if configfs_mounted() {
        return Ok(());
    }
    let _ = fs::create_dir_all(CONFIGFS);
    let src = CString::new("none").unwrap();
    let dst = CString::new(CONFIGFS).unwrap();
    let ty = CString::new("configfs").unwrap();
    let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty.as_ptr(), 0, std::ptr::null()) };
    if rc != 0 {
        return Err(format!("mount configfs: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn set_role(role: &Path, mode: &str) -> Result<String, String> {
    if write(role, &format!("{mode}\n")).is_err() {
        // Some MUSB builds only accept the OTG state names.
        let alt = if mode == "peripheral" { "b_peripheral" } else { "b_idle" };
        write(role, &format!("{alt}\n"))?;
    }
    Ok(fs::read_to_string(role).unwrap_or_default().trim().to_string())
}

fn create_gadget(function: &str) -> Result<(), String> {
    let g = Path::new(GADGET);
    mkdir(g)?;
    write(g.join("idVendor"), &format!("{:#06x}\n", gb350_proto::USB_VID))?;
    write(g.join("idProduct"), &format!("{:#06x}\n", gb350_proto::USB_PID))?;
    write(g.join("bcdDevice"), "0x0100\n")?;
    write(g.join("bcdUSB"), "0x0200\n")?;
    mkdir(g.join("strings/0x409"))?;
    write(g.join("strings/0x409/manufacturer"), "gb350\n")?;
    write(g.join("strings/0x409/product"), "gb350-agent\n")?;
    write(g.join("strings/0x409/serialnumber"), "gb350-0001\n")?;
    mkdir(g.join("configs/c.1"))?;
    mkdir(g.join("configs/c.1/strings/0x409"))?;
    write(g.join("configs/c.1/strings/0x409/configuration"), "agent\n")?;
    write(g.join("configs/c.1/MaxPower"), "100\n")?;
    mkdir(g.join("functions").join(function))?;
    Ok(())
}

fn add_mass_storage() -> Result<(), String> {
    let f = Path::new(GADGET).join("functions/mass_storage.0");
    mkdir(&f)?;
    write(f.join("lun.0/removable"), "1\n")?;
    let _ = write(f.join("lun.0/inquiry_string"), "GB350   SD card         0100\n");
    link_function("mass_storage.0")
}

/// Adds or removes the control channel at run time: the computer sees the
/// device reconnect.
pub fn set_control(on: bool) -> Result<(), String> {
    if on == control_on() {
        return Ok(());
    }
    let g = Path::new(GADGET);
    let udc_file = g.join("UDC");
    write(&udc_file, "\n")?;
    let r = if on {
        // The protocol must stay interface 0 (gbctl claims that): relink mass storage
        // after it.
        let ms = g.join("configs/c.1/mass_storage.0");
        let had_ms = ms.exists();
        if had_ms {
            let _ = fs::remove_file(&ms);
        }
        let r = link_function(FFS_FUNCTION);
        if had_ms {
            let _ = link_function("mass_storage.0");
        }
        r
    } else {
        fs::remove_file(g.join("configs/c.1").join(FFS_FUNCTION)).map_err(|e| format!("unlink {FFS_FUNCTION}: {e}"))
    };
    CONTROL.store(on && r.is_ok(), Ordering::Relaxed);
    write(&udc_file, &format!("{}\n", udc_name()))?;
    log::info(&format!("usb: control channel {}", if control_on() { "on" } else { "off" }));
    r
}

fn link_function(function: &str) -> Result<(), String> {
    let g = Path::new(GADGET);
    let link = g.join("configs/c.1").join(function);
    if !link.exists() {
        std::os::unix::fs::symlink(g.join("functions").join(function), &link)
            .map_err(|e| format!("symlink {}: {e}", link.display()))?;
    }
    Ok(())
}


/// Lists `bus-port` USB devices (excluding root hubs) as "id: vid:pid name".
fn usb_devices() -> Vec<String> {
    let read = |p: &Path, f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut out = Vec::new();
    for e in fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // Devices are "1-1", "1-1.2"; interfaces contain ':'; root hubs are "usbN".
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let p = e.path();
        out.push(format!(
            "{name}: {}:{} {} {} ({} Mbps)",
            read(&p, "idVendor"),
            read(&p, "idProduct"),
            read(&p, "manufacturer"),
            read(&p, "product"),
            read(&p, "speed")
        ));
    }
    out.sort();
    out
}

/// Temporarily turns the USB-C port into a host for `secs` seconds and logs
/// whatever enumerates, then restores the gadget. Runs in the background so
/// the request that started it can be answered first.
pub fn host_test(secs: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        let Some(role) = role_path() else {
            log::error("host test: no MUSB role control");
            return;
        };
        log::warn(&format!("host test: USB-C -> host for {secs}s; plug a USB device in"));
        let udc_file = Path::new(GADGET).join("UDC");
        let _ = write(&udc_file, "\n");
        match set_role(&role, "host") {
            Ok(r) => log::info(&format!("host test: role {r}")),
            Err(e) => log::error(&format!("host test: {e}")),
        }
        let mut seen: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            let now = usb_devices();
            for d in now.iter().filter(|d| !seen.contains(d)) {
                log::ok(&format!("host test: + {d}"));
            }
            for d in seen.iter().filter(|d| !now.contains(d)) {
                log::warn(&format!("host test: - {d}"));
            }
            seen = now;
            std::thread::sleep(Duration::from_millis(500));
        }
        if let Ok(k) = std::process::Command::new("dmesg").env("PATH", crate::sys::PATH).output() {
            let text = String::from_utf8_lossy(&k.stdout);
            for line in text.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev() {
                log::file_only(&format!("host test dmesg: {line}"));
            }
        }
        match set_role(&role, "peripheral") {
            Ok(r) => log::info(&format!("host test: role back to {r}")),
            Err(e) => log::error(&format!("host test: restore role: {e}")),
        }
        std::thread::sleep(Duration::from_millis(500));
        match write(&udc_file, &format!("{UDC}\n")) {
            Ok(()) => log::ok("host test: done - reconnect the Mac"),
            Err(e) => log::error(&format!("host test: rebind gadget: {e}")),
        }
    });
}

// FunctionFS (include/uapi/linux/usb/functionfs.h).
const FFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FFS_STRINGS_MAGIC: u32 = 2;
const FFS_HAS_FS_DESC: u32 = 1;
const FFS_HAS_HS_DESC: u32 = 2;

/// One vendor-class interface, bulk IN (ep1 file) then bulk OUT (ep2 file),
/// for full and high speed.
fn ffs_descriptors() -> Vec<u8> {
    let interface = [9u8, 4, 0, 0, 2, 0xff, 0, 0, 0];
    let endpoint = |addr: u8, mps: u16| -> [u8; 7] {
        let m = mps.to_le_bytes();
        [7, 5, addr, 2, m[0], m[1], 0]
    };
    let mut body = Vec::new();
    for mps in [64u16, 512] {
        body.extend_from_slice(&interface);
        body.extend_from_slice(&endpoint(0x81, mps));
        body.extend_from_slice(&endpoint(0x02, mps));
    }
    let mut out = Vec::new();
    for v in [FFS_DESCRIPTORS_MAGIC_V2, (20 + body.len()) as u32, FFS_HAS_FS_DESC | FFS_HAS_HS_DESC, 3, 3] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&body);
    out
}

/// No strings (iInterface = 0): FunctionFS accepts an empty table.
fn ffs_strings() -> Vec<u8> {
    [FFS_STRINGS_MAGIC, 16, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Brings the gadget up: mass storage always, the control channel if
/// `control`. Each step is logged so failures can be diagnosed from the
/// screen or the SD log.
pub fn setup(control: bool) -> Result<(), String> {
    CONTROL.store(control, Ordering::Relaxed);
    mount_configfs()?;
    let g = Path::new(GADGET);
    let udc_file = g.join("UDC");
    if fs::read_to_string(&udc_file).map(|s| !s.trim().is_empty()).unwrap_or(false) {
        log::ok("usb: gadget already bound (agent restarted)");
        return Ok(());
    }
    create_gadget(FFS_FUNCTION)?;

    mkdir(FFS_DIR)?;
    if !Path::new(FFS_DIR).join("ep0").exists() {
        let src = CString::new("gb350").unwrap();
        let dst = CString::new(FFS_DIR).unwrap();
        let ty = CString::new("functionfs").unwrap();
        let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty.as_ptr(), 0, std::ptr::null()) };
        if rc != 0 {
            return Err(format!("mount functionfs: {}", std::io::Error::last_os_error()));
        }
    }
    use std::io::Write;
    let mut ep0 = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(Path::new(FFS_DIR).join("ep0"))
        .map_err(|e| format!("open ep0: {e}"))?;
    ep0.write_all(&ffs_descriptors()).map_err(|e| format!("ep0 descriptors: {e}"))?;
    ep0.write_all(&ffs_strings()).map_err(|e| format!("ep0 strings: {e}"))?;
    log::info("usb: functionfs descriptors written");

    // ep0 must stay open for the function to exist; drain its events.
    std::thread::spawn(move || {
        use std::io::Read;
        const NAMES: [&str; 6] = ["bind", "unbind", "enable", "disable", "setup", "suspend"];
        let mut ev = [0u8; 12];
        while ep0.read_exact(&mut ev).is_ok() {
            let name = NAMES.get(ev[8] as usize).copied().unwrap_or("resume/other");
            log::file_only(&format!("usb: ffs event {name}"));
        }
    });

    if control {
        link_function(FFS_FUNCTION)?;
    }
    // USB storage mode (storage.rs): a removable drive with no medium until
    // the launcher asks; after the protocol interface, so gbctl's stays interface 0.
    if let Err(e) = add_mass_storage() {
        log::warn(&format!("usb: no mass storage: {e}"));
    }
    let udc = udc_name();
    write(&udc_file, &format!("{udc}\n"))?;
    log::ok(&format!("usb: functionfs gadget bound to {udc} ({:04x}:{:04x})", gb350_proto::USB_VID, gb350_proto::USB_PID));
    Ok(())
}

/// Opens the bulk pipe: `(rx, tx, description)`. `GB350_DEV` overrides it
/// with a single read/write file (off-device testing).
pub fn open_transport() -> std::io::Result<(fs::File, fs::File, String)> {
    if let Ok(path) = std::env::var("GB350_DEV") {
        let f = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        return Ok((f.try_clone()?, f, path));
    }
    let tx = fs::OpenOptions::new().write(true).open(Path::new(FFS_DIR).join("ep1"))?;
    let rx = fs::OpenOptions::new().read(true).open(Path::new(FFS_DIR).join("ep2"))?;
    Ok((rx, tx, format!("{FFS_DIR}/ep1+ep2")))
}
