//! The game file as the core wants it. Most games on the card are zipped
//! single ROMs (the stock layout), which most cores can't open: those are
//! unpacked in memory and handed over as data, under the inner file's name
//! (cores look at the extension). Cores that open archives themselves -
//! arcade romsets, `block_extract` or `zip` among their extensions - get the
//! zip's path. A core that insists on a path (`need_fullpath`) gets the
//! unpacked file in /tmp.

use crate::libretro as lr;
use std::ffi::{CStr, CString};

pub struct Content {
    pub path: CString,
    pub data: Option<Vec<u8>>,
}

impl Content {
    pub fn game_info(&self) -> lr::GameInfo {
        let (data, size) = match &self.data {
            Some(d) => (d.as_ptr() as *const _, d.len()),
            None => (std::ptr::null(), 0),
        };
        lr::GameInfo { path: self.path.as_ptr(), data, size, meta: std::ptr::null() }
    }
}

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub fn prepare(game: &str, si: &lr::SystemInfo) -> Result<Content, String> {
    let exts: Vec<String> = if si.valid_extensions.is_null() {
        vec![]
    } else {
        unsafe { CStr::from_ptr(si.valid_extensions) }.to_string_lossy().split('|').map(|e| e.to_ascii_lowercase()).collect()
    };
    let path = |p: &str| CString::new(p).map_err(|e| e.to_string());
    if ext(game) != "zip" || si.block_extract || exts.iter().any(|e| e == "zip") {
        let data = if si.need_fullpath { None } else { Some(std::fs::read(game).map_err(|e| format!("{game}: {e}"))?) };
        return Ok(Content { path: path(game)?, data });
    }
    let zip = std::fs::read(game).map_err(|e| format!("{game}: {e}"))?;
    let entries = zip_entries(&zip)?;
    let entry = entries
        .iter()
        .find(|e| exts.contains(&ext(&e.name)))
        .or_else(|| entries.iter().find(|e| !e.name.ends_with('/')))
        .ok_or_else(|| format!("{game}: empty zip"))?;
    let data = unzip(&zip, entry)?;
    let inner = entry.name.rsplit('/').next().unwrap_or(&entry.name).to_string();
    eprintln!("retro: {inner} from the zip ({} bytes)", data.len());
    if si.need_fullpath {
        let dir = "/tmp/retro-content";
        let _ = std::fs::create_dir_all(dir);
        let p = format!("{dir}/{inner}");
        std::fs::write(&p, &data).map_err(|e| format!("{p}: {e}"))?;
        return Ok(Content { path: path(&p)?, data: None });
    }
    // Next to the zip, under the inner name: what the core sees as the path.
    let dir = std::path::Path::new(game).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Content { path: path(&format!("{dir}/{inner}"))?, data: Some(data) })
}

struct Entry {
    name: String,
    method: u16,
    compressed: usize,
    size: usize,
    local: usize,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// The central directory's entries.
fn zip_entries(z: &[u8]) -> Result<Vec<Entry>, String> {
    let bad = || "not a zip file (or damaged)".to_string();
    // End of central directory: the last "PK\5\6", within 64 KiB of the end.
    let from = z.len().saturating_sub(22 + 65535);
    let eocd = (from..z.len().saturating_sub(21)).rev().find(|&i| z[i..i + 4] == *b"PK\x05\x06").ok_or_else(bad)?;
    let count = u16_at(z, eocd + 10).ok_or_else(bad)? as usize;
    let mut off = u32_at(z, eocd + 16).ok_or_else(bad)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if z.get(off..off + 4) != Some(b"PK\x01\x02") {
            return Err(bad());
        }
        let name_len = u16_at(z, off + 28).ok_or_else(bad)? as usize;
        let extra = u16_at(z, off + 30).ok_or_else(bad)? as usize;
        let comment = u16_at(z, off + 32).ok_or_else(bad)? as usize;
        out.push(Entry {
            name: String::from_utf8_lossy(z.get(off + 46..off + 46 + name_len).ok_or_else(bad)?).into_owned(),
            method: u16_at(z, off + 10).ok_or_else(bad)?,
            compressed: u32_at(z, off + 20).ok_or_else(bad)? as usize,
            size: u32_at(z, off + 24).ok_or_else(bad)? as usize,
            local: u32_at(z, off + 42).ok_or_else(bad)? as usize,
        });
        off += 46 + name_len + extra + comment;
    }
    Ok(out)
}

fn unzip(z: &[u8], e: &Entry) -> Result<Vec<u8>, String> {
    let bad = || format!("{}: damaged zip entry", e.name);
    if z.get(e.local..e.local + 4) != Some(b"PK\x03\x04") {
        return Err(bad());
    }
    let start = e.local + 30 + u16_at(z, e.local + 26).ok_or_else(bad)? as usize + u16_at(z, e.local + 28).ok_or_else(bad)? as usize;
    let raw = z.get(start..start + e.compressed).ok_or_else(bad)?;
    let data = match e.method {
        0 => raw.to_vec(),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, e.size.max(1)).map_err(|err| format!("{}: inflate: {err:?}", e.name))?,
        m => return Err(format!("{}: unsupported zip method {m}", e.name)),
    };
    Ok(data)
}
