//! Settings (saved on the card as `key=value` lines) and the hardware they
//! drive: backlight (sysfs), volume (ALSA controls; 0 % also switches the
//! speaker off - `gb350_pcm::set_volume`), plus how games are shown (passed
//! to the emulator).

use std::path::{Path, PathBuf};

const BACKLIGHT: &str = "/sys/class/backlight/backlight";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    Aspect,
    Integer,
    Stretch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Smooth,
    Sharp,
}

/// The colour around a scaled game (the display's background).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Border {
    Black,
    Grey,
    Blue,
}

impl Border {
    pub fn rgb(self) -> u32 {
        match self {
            Border::Black => 0x000000,
            Border::Grey => 0x2a2c31,
            Border::Blue => 0x101a33,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Border::Black => "black",
            Border::Grey => "grey",
            Border::Blue => "blue",
        }
    }
}

pub struct Settings {
    path: PathBuf,
    /// Percent, 10..=100.
    pub brightness: u8,
    /// Percent, 0..=100 (0: speaker off and muted).
    pub volume: u8,
    pub scaling: Scaling,
    pub filter: Filter,
    /// Frames per second and CPU use over games (the emulator reads it too).
    pub perf: bool,
    /// Vertical games: false upright on the wide screen, true sideways (turn
    /// the console). The emulator reads it (`vertical`).
    pub sideways: bool,
    /// PlayStation games: drawing shared with the second core (true) or all
    /// on the main core. The emulator reads it (`ps1_cores` = 2 | 1).
    pub ps1_two_cores: bool,
    /// Picture (the display hardware's colour words, everywhere): 0..=100,
    /// 50 normal; sharpness 0..=10, 5 normal.
    pub colour: u8,
    pub contrast: u8,
    pub sharpness: u8,
    pub border: Border,
}

pub use gb350_pcm::VOLUME_STEP;

impl Settings {
    pub fn load(path: PathBuf) -> Settings {
        // Quiet defaults.
        let mut s = Settings {
            path,
            brightness: 60,
            volume: 10,
            scaling: Scaling::Aspect,
            filter: Filter::Smooth,
            perf: false,
            sideways: false,
            ps1_two_cores: true,
            colour: 50,
            contrast: 50,
            sharpness: 5,
            border: Border::Black,
        };
        let text = std::fs::read_to_string(&s.path).unwrap_or_default();
        // Before 2026-10-17: `volume` was the hardware gain (0-255) and
        // `speaker` a separate auto/off/on; `speaker=off` becomes 0 %.
        let (mut pct, mut gain, mut speaker_off) = (None, None, false);
        for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
            let v = v.trim();
            match k.trim() {
                "brightness" => s.brightness = v.parse().unwrap_or(60u8).clamp(10, 100),
                "volume_pct" => pct = v.parse::<u8>().ok(),
                "volume" => gain = v.parse::<u32>().ok(),
                "speaker" => speaker_off = v == "off",
                "scaling" => s.scaling = match v { "integer" => Scaling::Integer, "stretch" => Scaling::Stretch, _ => Scaling::Aspect },
                "filter" => s.filter = if v == "sharp" { Filter::Sharp } else { Filter::Smooth },
                "perf" => s.perf = v == "on",
                "vertical" => s.sideways = v == "sideways",
                "ps1_cores" => s.ps1_two_cores = v != "1",
                "colour" => s.colour = v.parse().unwrap_or(50u8).min(100),
                "contrast" => s.contrast = v.parse().unwrap_or(50u8).min(100),
                "sharpness" => s.sharpness = v.parse().unwrap_or(5u8).min(10),
                "border" => s.border = match v { "grey" => Border::Grey, "blue" => Border::Blue, _ => Border::Black },
                _ => {}
            }
        }
        s.volume = match (pct, gain) {
            (Some(p), _) => p.min(100),
            (None, _) if speaker_off => 0,
            (None, Some(g)) => ((g.min(255) * 100 + 127) / 255) as u8,
            (None, None) => s.volume,
        };
        s
    }

    pub fn save(&self) {
        // Not while the card is away (USB storage): it would land in the
        // empty mount point.
        if !self.path.parent().and_then(Path::parent).is_some_and(Path::exists) {
            return;
        }
        let text = format!(
            "brightness={}\nvolume_pct={}\nscaling={}\nfilter={}\nperf={}\nvertical={}\nps1_cores={}\ncolour={}\ncontrast={}\nsharpness={}\nborder={}\n",
            self.brightness,
            self.volume,
            self.scaling_name(),
            if self.filter == Filter::Sharp { "sharp" } else { "smooth" },
            if self.perf { "on" } else { "off" },
            if self.sideways { "sideways" } else { "upright" },
            if self.ps1_two_cores { 2 } else { 1 },
            self.colour,
            self.contrast,
            self.sharpness,
            self.border.name(),
        );
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = std::fs::write(&self.path, text) {
            eprintln!("launcher: {}: {e}", self.path.display());
        }
    }

    pub fn scaling_name(&self) -> &'static str {
        match self.scaling {
            Scaling::Aspect => "aspect",
            Scaling::Integer => "integer",
            Scaling::Stretch => "stretch",
        }
    }

    pub fn apply_all(&self) {
        self.apply_brightness();
        self.apply_volume();
        self.apply_picture();
    }

    /// Colour, contrast, sharpness and the border colour, in the display
    /// hardware (games included: the emulator uses the same layers).
    pub fn apply_picture(&self) {
        use gb350_kms::control;
        let p = control::Picture {
            saturation: self.colour as u32,
            contrast: self.contrast as u32,
            sharpness: self.sharpness as u32,
            ..Default::default()
        };
        if let Err(e) = control::set_picture(&p).and_then(|_| control::set_background(self.border.rgb())) {
            eprintln!("launcher: picture: {e}");
        }
    }

    /// Backlight off (screen off); `apply_brightness` brings it back.
    pub fn backlight_off(&self) {
        let _ = std::fs::write(format!("{BACKLIGHT}/brightness"), "0");
    }

    pub fn apply_brightness(&self) {
        let max: u32 = std::fs::read_to_string(format!("{BACKLIGHT}/max_brightness")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(255);
        let v = max * self.brightness as u32 / 100;
        if let Err(e) = std::fs::write(format!("{BACKLIGHT}/brightness"), v.to_string()) {
            eprintln!("launcher: backlight: {e}");
        }
    }

    pub fn apply_volume(&self) {
        if let Err(e) = gb350_pcm::set_volume(self.volume) {
            eprintln!("launcher: volume: {e}");
        }
    }

    /// Re-reads the file (the in-game menu writes volume, brightness,
    /// scaling and filter back to it).
    pub fn reload(&mut self) {
        *self = Settings::load(self.path.clone());
    }

    pub fn volume_pct(&self) -> u32 {
        self.volume as u32
    }

    /// Emulator environment for these settings.
    pub fn retro_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("RETRO_VOLUME", self.volume.to_string()),
            ("RETRO_SCALE", self.scaling_name().to_string()),
            ("RETRO_SETTINGS", self.path.to_string_lossy().into_owned()),
        ];
        if self.filter == Filter::Sharp {
            env.push(("RETRO_FILTER", "nearest".to_string()));
        }
        env
    }
}

// ---- read-only system facts for the settings screen ----

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Live figures for Settings -> System: the shared sample plus the second
/// core's state.
#[derive(Clone, Default)]
pub struct Stats {
    pub now: gb350_ui::sysstat::Sample,
    /// The second core's state ("running", ...).
    pub core: String,
}

#[derive(Default)]
pub struct Sampler(gb350_ui::sysstat::Sampler);

impl Sampler {
    pub fn sample(&mut self) -> Stats {
        let core = read_trim("/sys/devices/platform/bd00000.syscore/state").unwrap_or_else(|| "not found".into());
        Stats { now: self.0.sample(), core }
    }
}

/// (percent, status) of the battery.
pub fn battery() -> Option<(u32, String)> {
    let cap = read_trim("/sys/class/power_supply/battery/capacity")?.parse().ok()?;
    let status = read_trim("/sys/class/power_supply/battery/status").unwrap_or_default();
    Some((cap, status))
}

/// (free, total) bytes on the card.
pub fn card_space(root: &std::path::Path) -> Option<(u64, u64)> {
    let p = std::ffi::CString::new(root.to_string_lossy().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(p.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64 * st.f_frsize as u64, st.f_blocks as u64 * st.f_frsize as u64))
}

pub fn kernel_release() -> String {
    read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "?".into())
}

/// The second core's firmware version (`gb350-syscore 0.2.1`).
pub fn syscore_version() -> String {
    read_trim("/sys/devices/platform/bd00000.syscore/info")
        .and_then(|s| s.lines().next().and_then(|l| l.split_whitespace().nth(1)).map(String::from))
        .unwrap_or_else(|| "not running".into())
}
