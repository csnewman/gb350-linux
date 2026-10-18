//! Button and headphone state, asked from the kernel's input devices on
//! demand (the second core scans the buttons; `gb350-syscore-keys` reports
//! them as `gb350-buttons`).

use std::os::fd::AsRawFd;

/// EVIOCGKEY(96) / EVIOCGSW(8), MIPS ioctl encoding (read = 2 << 29).
const EVIOCGKEY: u32 = (2 << 29) | (96 << 16) | (0x45 << 8) | 0x18;
const EVIOCGSW: u32 = (2 << 29) | (8 << 16) | (0x45 << 8) | 0x1b;
const SW_HEADPHONE_INSERT: usize = 2;

/// Labels as printed on the case (A is BTN_EAST, B is BTN_SOUTH).
const NAMES: &[(usize, &str)] = &[
    (0x220, "Up"),
    (0x221, "Down"),
    (0x222, "Left"),
    (0x223, "Right"),
    (0x131, "A"),
    (0x130, "B"),
    (0x133, "X"),
    (0x134, "Y"),
    (0x136, "L1"),
    (0x137, "R1"),
    (0x138, "L2"),
    (0x139, "R2"),
    (0x13a, "Select"),
    (0x13b, "Start"),
    (0x13c, "Fn"),
    (114, "Vol-"),
    (115, "Vol+"),
    (116, "Power"),
];

fn query<const N: usize>(req: u32) -> Vec<[u8; N]> {
    let mut out = vec![];
    for e in std::fs::read_dir("/dev/input").into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        if let Ok(f) = std::fs::File::open(e.path()) {
            let mut bits = [0u8; N];
            if unsafe { libc::ioctl(f.as_raw_fd(), req as _, bits.as_mut_ptr()) } >= 0 {
                out.push(bits);
            }
        }
    }
    out
}

/// Buttons held right now.
pub fn held() -> Vec<&'static str> {
    let devs = query::<96>(EVIOCGKEY);
    NAMES.iter().filter(|(code, _)| devs.iter().any(|b| b[code / 8] >> (code % 8) & 1 != 0)).map(|(_, n)| *n).collect()
}

pub fn headphones() -> bool {
    query::<8>(EVIOCGSW).iter().any(|b| b[SW_HEADPHONE_INSERT / 8] >> (SW_HEADPHONE_INSERT % 8) & 1 != 0)
}

/// Battery level and state (`/sys/class/power_supply/battery`).
pub fn battery() -> String {
    let read = |f: &str| std::fs::read_to_string(format!("/sys/class/power_supply/battery/{f}")).map(|s| s.trim().to_string());
    match (read("capacity"), read("status")) {
        (Ok(c), Ok(s)) => format!("{c}% {}", s.to_lowercase()),
        _ => "battery n/a".into(),
    }
}
