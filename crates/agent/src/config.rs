//! `gb350/agent.conf` on the card: `key=value` lines. Keys:
//! - `usb_control=on|off`: the USB control channel (`gbctl`: shell, files,
//!   memory access - root over a cable). Off unless set; the launcher's
//!   Settings switch it (About -> Developer).

use crate::sys;

fn path() -> std::path::PathBuf {
    sys::base().join("agent.conf")
}

fn get(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path()).ok()?;
    text.lines().filter_map(|l| l.split_once('=')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string())
}

fn set(key: &str, value: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().filter(|l| l.split('=').next().map(str::trim) != Some(key)).map(String::from).collect();
    lines.push(format!("{key}={value}"));
    std::fs::write(path(), lines.join("\n") + "\n").map_err(|e| format!("{}: {e}", path().display()))
}

pub fn usb_control() -> bool {
    get("usb_control").as_deref() == Some("on")
}

pub fn set_usb_control(on: bool) -> Result<(), String> {
    set("usb_control", if on { "on" } else { "off" })
}
