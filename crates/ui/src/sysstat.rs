//! System figures for Settings -> System and the in-game stats overlay:
//! Linux's CPU use (/proc/stat), the second core's (its idle-cycle counter,
//! `bd00000.syscore/load`), memory in use and uptime. Load figures compare
//! two samples, so the first one has none.

const LOAD: &str = "/sys/devices/platform/bd00000.syscore/load";

#[derive(Clone, Default, Debug)]
pub struct Sample {
    /// Percent busy since the previous sample.
    pub cpu: Option<u32>,
    pub second_core: Option<u32>,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub uptime_s: u64,
}

#[derive(Default)]
pub struct Sampler {
    cpu: Option<(u64, u64)>,
    core: Option<(u32, u32)>,
}

fn cpu_times() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let v: Vec<u64> = text.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let total: u64 = v.iter().sum();
    Some((total - v.get(3).copied().unwrap_or(0) - v.get(4).copied().unwrap_or(0), total))
}

/// (idle cycles, count) of the second core.
fn core_times() -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(LOAD).ok()?;
    let mut f = text.split_whitespace().filter_map(|x| x.parse().ok());
    Some((f.next()?, f.next()?))
}

impl Sampler {
    pub fn sample(&mut self) -> Sample {
        let cpu_now = cpu_times();
        let cpu = match (self.cpu, cpu_now) {
            (Some((b0, t0)), Some((b1, t1))) if t1 > t0 => Some(((b1 - b0) * 100 / (t1 - t0)) as u32),
            _ => None,
        };
        self.cpu = cpu_now;
        let core_now = core_times();
        let second_core = match (self.core, core_now) {
            (Some((i0, c0)), Some((i1, c1))) if c1 != c0 => {
                let (idle, span) = (i1.wrapping_sub(i0) as u64, c1.wrapping_sub(c0) as u64);
                Some(100 - (idle * 100 / span).min(100) as u32)
            }
            _ => None,
        };
        self.core = core_now;
        let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let kb = |key: &str| -> u64 { mem.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0) };
        let (total, avail) = (kb("MemTotal:"), kb("MemAvailable:"));
        let uptime_s = std::fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split('.').next()?.parse().ok()).unwrap_or(0);
        Sample { cpu, second_core, mem_used_mb: ((total - avail) / 1024) as u32, mem_total_mb: (total / 1024) as u32, uptime_s }
    }
}
