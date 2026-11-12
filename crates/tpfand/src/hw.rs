//! Hardware access: temperature sensors, ThinkPad fan, Intel RAPL.

use crate::config::Res;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tpfand_proto::Limits;

pub const FAN_PATH: &str = "/proc/acpi/ibm/fan";
pub const PROFILE_PATH: &str = "/sys/firmware/acpi/platform_profile";
pub const RAPL_DOMAINS: &[&str] = &[
    "/sys/class/powercap/intel-rapl:0",
    "/sys/class/powercap/intel-rapl-mmio:0",
];

pub fn read_trim(p: &Path) -> std::io::Result<String> {
    Ok(fs::read_to_string(p)?.trim().to_string())
}

fn read_u64(p: &Path) -> Option<u64> {
    read_trim(p).ok()?.parse().ok()
}

// ---------------------------------------------------------------- sensors --

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Hottest core (spikes to ~100°C on single-core boost; safety only).
    Peak,
    /// Per-core reading, averaged into the control temperature.
    Core,
    /// Counts directly toward the control temperature (shared heatsink).
    Control,
    /// Shown in status only.
    Info,
}

pub struct Sensors {
    inputs: Vec<(String, Kind, PathBuf)>,
    pub fans: Vec<PathBuf>,
}

pub struct Readings {
    /// What gets shown to the user.
    pub display: Vec<(String, f64)>,
    /// Drives fan curves + dynamic boost: max(avg of cores, dGPU).
    pub control: Option<f64>,
    /// Hottest reading of anything (critical_instant check).
    pub peak: Option<f64>,
}

impl Sensors {
    pub fn discover() -> Res<Self> {
        let mut inputs = Vec::new();
        let mut fans = Vec::new();
        for entry in fs::read_dir("/sys/class/hwmon")? {
            let dir = entry?.path();
            let name = read_trim(&dir.join("name")).unwrap_or_default();
            match name.as_str() {
                "coretemp" => {
                    for (label, input) in labelled_temps(&dir) {
                        if label.starts_with("Package") {
                            inputs.push(("CPU hottest core".to_string(), Kind::Peak, input));
                        } else if label.starts_with("Core") {
                            inputs.push((label, Kind::Core, input));
                        }
                    }
                }
                "thinkpad" => {
                    for (label, input) in labelled_temps(&dir) {
                        match label.as_str() {
                            "CPU" => inputs.push(("CPU (EC)".into(), Kind::Info, input)),
                            "GPU" => inputs.push(("GPU".into(), Kind::Control, input)),
                            _ => {}
                        }
                    }
                    for i in 1..=4 {
                        let f = dir.join(format!("fan{i}_input"));
                        if f.exists() {
                            fans.push(f);
                        }
                    }
                }
                _ => {}
            }
        }
        if !inputs.iter().any(|(_, k, _)| matches!(k, Kind::Core | Kind::Control | Kind::Peak)) {
            return Err("no coretemp/thinkpad temperature sensors found".into());
        }
        Ok(Sensors { inputs, fans })
    }

    pub fn names(&self) -> String {
        let cores = self.inputs.iter().filter(|(_, k, _)| *k == Kind::Core).count();
        let mut v: Vec<String> = self
            .inputs
            .iter()
            .filter(|(_, k, _)| *k != Kind::Core)
            .map(|(l, _, _)| l.clone())
            .collect();
        v.push(format!("{cores} cores"));
        v.join(", ")
    }

    /// Valid readings only (unpowered dGPU / absent sensors report 0, 1, -128).
    pub fn read(&self) -> Readings {
        let mut display = Vec::new();
        let (mut core_sum, mut core_n) = (0.0, 0);
        let mut control: Option<f64> = None;
        let mut peak: Option<f64> = None;
        for (label, kind, path) in &self.inputs {
            let Some(t) = read_trim(path)
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|m| m / 1000.0)
                .filter(|t| (10.0..=125.0).contains(t))
            else {
                continue;
            };
            peak = Some(peak.map_or(t, |p| p.max(t)));
            match kind {
                Kind::Core => {
                    core_sum += t;
                    core_n += 1;
                }
                Kind::Control => {
                    control = Some(control.map_or(t, |c| c.max(t)));
                    display.push((label.clone(), t));
                }
                Kind::Peak | Kind::Info => display.push((label.clone(), t)),
            }
        }
        if core_n > 0 {
            let avg = core_sum / core_n as f64;
            display.insert(0, ("CPU core average".into(), avg));
            control = Some(control.map_or(avg, |c| c.max(avg)));
        }
        // No per-core sensors at all: fall back to the peak.
        let control = control.or(peak);
        Readings { display, control, peak }
    }

    pub fn rpm(&self) -> Vec<u32> {
        self.fans.iter().filter_map(|p| read_u64(p).map(|v| v as u32)).collect()
    }
}

fn labelled_temps(dir: &Path) -> Vec<(String, PathBuf)> {
    (1..=32)
        .filter_map(|i| {
            let input = dir.join(format!("temp{i}_input"));
            let label = read_trim(&dir.join(format!("temp{i}_label"))).ok()?;
            input.exists().then_some((label, input))
        })
        .collect()
}

// -------------------------------------------------------------------- fan --

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fan {
    Level(u8),
    FullSpeed,
    Auto,
}

impl Fan {
    pub fn cmd(self) -> String {
        match self {
            Fan::Level(l) => format!("level {l}"),
            Fan::FullSpeed => "level full-speed".into(),
            Fan::Auto => "level auto".into(),
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            Fan::Level(l) => l,
            Fan::Auto => 0,
            Fan::FullSpeed => 8,
        }
    }
}

pub fn fan_write(cmd: &str) -> std::io::Result<()> {
    fs::write(FAN_PATH, cmd)
}

pub fn system_profile() -> String {
    read_trim(Path::new(PROFILE_PATH)).unwrap_or_else(|_| "balanced".into())
}

// ------------------------------------------------------------------- RAPL --

/// Apply limits to every RAPL domain present; only writes values that differ.
/// Returns true if anything was written.
pub fn apply_rapl(l: Limits) -> bool {
    let mut changed = false;
    for dom in RAPL_DOMAINS.iter().map(Path::new).filter(|d| d.exists()) {
        for (idx, w) in [(0, l.pl1_w), (1, l.pl2_w)] {
            let file = dom.join(format!("constraint_{idx}_power_limit_uw"));
            let want = w as u64 * 1_000_000;
            if read_u64(&file) != Some(want) {
                match fs::write(&file, want.to_string()) {
                    Ok(()) => changed = true,
                    Err(e) => eprintln!("warn: write {}: {e}", file.display()),
                }
            }
        }
    }
    changed
}

/// Effective (lowest across domains) limits currently programmed.
pub fn read_rapl() -> Option<Limits> {
    let mut out: Option<Limits> = None;
    for dom in RAPL_DOMAINS.iter().map(Path::new).filter(|d| d.exists()) {
        let pl1 = read_u64(&dom.join("constraint_0_power_limit_uw"))? / 1_000_000;
        let pl2 = read_u64(&dom.join("constraint_1_power_limit_uw"))? / 1_000_000;
        let (pl1, pl2) = (pl1 as u32, pl2 as u32);
        out = Some(match out {
            None => Limits { pl1_w: pl1, pl2_w: pl2 },
            Some(o) => Limits { pl1_w: o.pl1_w.min(pl1), pl2_w: o.pl2_w.min(pl2) },
        });
    }
    out
}

/// Package power meter from RAPL energy counters.
pub struct PowerMeter {
    last: Option<(u64, Instant)>,
    range: u64,
    path: PathBuf,
}

impl PowerMeter {
    pub fn new() -> Self {
        let dom = Path::new(RAPL_DOMAINS[0]);
        PowerMeter {
            last: None,
            range: read_u64(&dom.join("max_energy_range_uj")).unwrap_or(u64::MAX),
            path: dom.join("energy_uj"),
        }
    }

    pub fn sample(&mut self) -> Option<f64> {
        let now = Instant::now();
        let e = read_u64(&self.path)?;
        let watts = self.last.and_then(|(pe, pt)| {
            let dt = now.duration_since(pt).as_secs_f64();
            let de = if e >= pe { e - pe } else { e + (self.range - pe) };
            (dt > 0.05).then(|| de as f64 / 1e6 / dt)
        });
        self.last = Some((e, now));
        watts
    }
}
