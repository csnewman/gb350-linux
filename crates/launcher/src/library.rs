//! The game library: one folder per system at the card root - the stock
//! firmware's layout (`PS/`, `SFC/`, ...), so a stock card's games are found
//! where they are - or, also read, under `roms/`. A folder's
//! `filelist.csv` (`file,English name,Chinese name`, UTF-8 with BOM) gives
//! display names; `images/<file stem>.png` a 320x240 screenshot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct System {
    /// Folder name on the card.
    pub dir: &'static str,
    /// Tab label.
    pub short: &'static str,
    pub name: &'static str,
    /// Emulator binary in gb350/bin (scripts/build-emulators.sh), if any.
    pub core: Option<&'static str>,
    /// Accepted file extensions (lower case); empty = any not ignored.
    pub exts: &'static [&'static str],
}

pub const SYSTEMS: &[System] = &[
    System { dir: "PS", short: "PS", name: "PlayStation", core: Some("retro-pcsx_rearmed"), exts: &["img", "iso", "bin", "cue", "chd", "pbp"] },
    System { dir: "SFC", short: "SFC", name: "Super Famicom", core: Some("retro-snes9x2005"), exts: &[] },
    System { dir: "GBA", short: "GBA", name: "Game Boy Advance", core: Some("retro-gpsp"), exts: &[] },
    System { dir: "FC", short: "FC", name: "Famicom", core: Some("retro-fceumm"), exts: &[] },
    System { dir: "MD", short: "MD", name: "Mega Drive", core: Some("retro-picodrive"), exts: &[] },
    System { dir: "PCE", short: "PCE", name: "PC Engine", core: Some("retro-mednafen_pce_fast"), exts: &[] },
    System { dir: "MAME", short: "ARC", name: "Arcade", core: Some("retro-fbalpha2012"), exts: &[] },
    System { dir: "GB", short: "GB", name: "Game Boy", core: Some("retro-gambatte"), exts: &[] },
    System { dir: "GBC", short: "GBC", name: "Game Boy Color", core: Some("retro-gambatte"), exts: &[] },
    System { dir: "GG", short: "GG", name: "Game Gear", core: Some("retro-picodrive"), exts: &[] },
    System { dir: "SMS", short: "SMS", name: "Master System", core: Some("retro-picodrive"), exts: &[] },
    System { dir: "NGPC", short: "NGPC", name: "Neo Geo Pocket Color", core: Some("retro-mednafen_ngp"), exts: &[] },
    System { dir: "WSC", short: "WSC", name: "WonderSwan Color", core: Some("retro-mednafen_wswan"), exts: &[] },
    System { dir: "ATARI", short: "2600", name: "Atari 2600", core: Some("retro-stella2014"), exts: &[] },
];

/// Files in system folders that are not games.
const IGNORED: &[&str] = &["csv", "lst", "txt", "png", "jpg", "xml", "sav", "srm", "state", "ini", "cfg", "db"];
/// BIOS archives the arcade emulator needs next to the games (not games).
const BIOS: &[&str] = &["neogeo.zip", "pgm.zip", "decocass.zip", "isgsm.zip", "nmk004.zip", "skns.zip", "ym2608.zip", "cchip.zip", "bubsys.zip", "namcoc69.zip", "namcoc70.zip", "namcoc75.zip"];

pub struct Game {
    pub sys: usize,
    /// Relative to the card root, e.g. `PS/Crash Bandicoot 1.img`.
    pub path: String,
    pub name: String,
    /// Lower-case name, for sorting and search.
    pub key: String,
    /// Jump-strip group: an upper-case letter, or '#'.
    pub letter: char,
}

/// A list the library screen can show.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tab {
    Recent,
    Favourites,
    System(usize),
}

pub struct Library {
    pub root: PathBuf,
    pub games: Vec<Game>,
    /// Per system, sorted by name.
    pub by_sys: Vec<Vec<u32>>,
    index: HashMap<String, u32>,
    pub favourites: Vec<String>,
    /// Most recent first.
    pub recent: Vec<String>,
}

const RECENT_MAX: usize = 30;

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

/// `filelist.csv`: file name -> English display name.
fn names(dir: &Path) -> HashMap<String, String> {
    let Ok(bytes) = std::fs::read(dir.join("filelist.csv")) else { return HashMap::new() };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, ',');
            let (file, name) = (f.next()?.trim(), f.next()?.trim());
            (!file.is_empty() && !name.is_empty()).then(|| (file.to_string(), name.to_string()))
        })
        .collect()
}

fn letter(name: &str) -> char {
    match name.chars().find(|c| c.is_alphanumeric()) {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

impl Library {
    pub fn data_dir(root: &Path) -> PathBuf {
        root.join("gb350/launcher")
    }

    pub fn scan(root: &Path) -> Library {
        let mut games = vec![];
        let mut by_sys = vec![vec![]; SYSTEMS.len()];
        for (si, sys) in SYSTEMS.iter().enumerate() {
            for parent in ["", "roms/"] {
                let rel = format!("{parent}{}", sys.dir);
                let dir = root.join(&rel);
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                let names = names(&dir);
                let files: Vec<String> = rd
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.'))
                    .collect();
                // Disc images: a .bin belongs to its .cue.
                let cues: Vec<String> = files.iter().filter(|n| ext(n) == "cue").map(|n| stem(n).to_ascii_lowercase()).collect();
                for f in files {
                    let e = ext(&f);
                    if IGNORED.contains(&e.as_str()) || (!sys.exts.is_empty() && !sys.exts.contains(&e.as_str())) {
                        continue;
                    }
                    if e == "bin" && cues.contains(&stem(&f).to_ascii_lowercase()) {
                        continue;
                    }
                    if BIOS.contains(&f.to_ascii_lowercase().as_str()) {
                        continue;
                    }
                    let name = names.get(&f).cloned().unwrap_or_else(|| stem(&f).to_string());
                    by_sys[si].push(games.len() as u32);
                    games.push(Game { sys: si, path: format!("{rel}/{f}"), key: name.to_lowercase(), letter: letter(&name), name });
                }
            }
            by_sys[si].sort_by(|&a, &b| games[a as usize].key.cmp(&games[b as usize].key));
        }
        let index = games.iter().enumerate().map(|(i, g)| (g.path.clone(), i as u32)).collect();
        let data = Self::data_dir(root);
        let read_list = |f: &str| -> Vec<String> {
            std::fs::read_to_string(data.join(f)).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
        };
        Library { root: root.to_path_buf(), games, by_sys, index, favourites: read_list("favourites.txt"), recent: read_list("recent.txt") }
    }

    pub fn find(&self, path: &str) -> Option<u32> {
        self.index.get(path).copied()
    }

    /// The games a tab shows (systems and favourites sorted by name; recent
    /// in play order). Missing files are left out.
    pub fn list(&self, tab: Tab) -> Vec<u32> {
        match tab {
            Tab::System(s) => self.by_sys[s].clone(),
            Tab::Recent => self.recent.iter().filter_map(|p| self.find(p)).collect(),
            Tab::Favourites => {
                let mut v: Vec<u32> = self.favourites.iter().filter_map(|p| self.find(p)).collect();
                v.sort_by(|&a, &b| self.games[a as usize].key.cmp(&self.games[b as usize].key));
                v
            }
        }
    }

    /// Tabs with something in them: recent, favourites, then systems.
    pub fn tabs(&self) -> Vec<Tab> {
        let mut t = vec![];
        if !self.list(Tab::Recent).is_empty() {
            t.push(Tab::Recent);
        }
        if !self.list(Tab::Favourites).is_empty() {
            t.push(Tab::Favourites);
        }
        t.extend((0..SYSTEMS.len()).filter(|&s| !self.by_sys[s].is_empty()).map(Tab::System));
        t
    }

    pub fn is_favourite(&self, g: u32) -> bool {
        let p = &self.games[g as usize].path;
        self.favourites.iter().any(|f| f == p)
    }

    pub fn toggle_favourite(&mut self, g: u32) {
        let p = self.games[g as usize].path.clone();
        if let Some(i) = self.favourites.iter().position(|f| *f == p) {
            self.favourites.remove(i);
        } else {
            self.favourites.push(p);
        }
        self.save("favourites.txt", &self.favourites);
    }

    pub fn played(&mut self, g: u32) {
        let p = self.games[g as usize].path.clone();
        self.recent.retain(|r| *r != p);
        self.recent.insert(0, p);
        self.recent.truncate(RECENT_MAX);
        self.save("recent.txt", &self.recent);
    }

    fn save(&self, file: &str, list: &[String]) {
        if !self.root.join("gb350").exists() {
            return; // the card is away (USB storage)
        }
        let dir = Self::data_dir(&self.root);
        let _ = std::fs::create_dir_all(&dir);
        let mut text = list.join("\n");
        text.push('\n');
        if let Err(e) = std::fs::write(dir.join(file), text) {
            eprintln!("launcher: {file}: {e}");
        }
    }

    pub fn full_path(&self, g: u32) -> PathBuf {
        self.root.join(&self.games[g as usize].path)
    }

    /// The stock screenshot for a game, if there is one.
    pub fn image_path(&self, g: u32) -> PathBuf {
        let game = &self.games[g as usize];
        let (dir, file) = game.path.rsplit_once('/').unwrap_or(("", &game.path));
        self.root.join(dir).join("images").join(format!("{}.png", stem(file)))
    }

    /// Upper-case file extension (shown as the format).
    pub fn format(&self, g: u32) -> String {
        ext(&self.games[g as usize].path).to_ascii_uppercase()
    }

    pub fn total(&self) -> usize {
        self.games.len()
    }

    pub fn systems_used(&self) -> usize {
        self.by_sys.iter().filter(|v| !v.is_empty()).count()
    }
}

/// Search: games whose name contains every word of `query`, best first
/// (name starts with the query, then a word starts with it, then anywhere;
/// shorter names first). `sys` limits it to one system.
pub fn search(lib: &Library, query: &str, sys: Option<usize>, max: usize) -> Vec<u32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<(u8, usize, u32)> = vec![];
    for (i, g) in lib.games.iter().enumerate() {
        if sys.is_some_and(|s| s != g.sys) {
            continue;
        }
        if !words.iter().all(|w| g.key.contains(w)) {
            continue;
        }
        let rank = if g.key.starts_with(&q) {
            0
        } else if g.key.contains(&format!(" {q}")) {
            1
        } else {
            2
        };
        hits.push((rank, g.key.len(), i as u32));
    }
    hits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| lib.games[a.2 as usize].key.cmp(&lib.games[b.2 as usize].key)));
    hits.truncate(max);
    hits.into_iter().map(|h| h.2).collect()
}

/// Human-readable size.
pub fn size_text(bytes: u64) -> String {
    const MB: u64 = 1 << 20;
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= MB {
        format!("{} MB", (bytes + MB / 2) / MB)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}
