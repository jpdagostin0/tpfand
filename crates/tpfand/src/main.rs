//! tpfand — profile-aware fan curve + power limit daemon for ThinkPads.
//!
//! Follows the ACPI platform profile (set by power-profiles-daemon) or a
//! profile pinned from the panel, and applies a per-profile fan curve via
//! /proc/acpi/ibm/fan plus per-profile Intel RAPL package power limits.
//! Forces full-speed at critical temperatures regardless of settings.

mod boost;
mod config;
mod control;
mod gpu;
mod hw;
mod ipc;

use config::{Config, Res};
use boost::Boost;
use control::Controller;
use gpu::{Gpu, GpuSample};
use hw::{Fan, PowerMeter, Sensors};
use ipc::{Shared, UserState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tpfand_proto::{Client, FanMode, Status};

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn run(cfg: Config) -> Res<()> {
    if !is_root() {
        return Err("tpfand must run as root".into());
    }
    let cfg = Arc::new(cfg);
    let sensors = Sensors::discover()?;
    eprintln!("tpfand: sensors: {}", sensors.names());
    hw::fan_write(&format!("watchdog {}", cfg.watchdog_s)).map_err(|e| {
        format!("cannot control fan ({e}); load thinkpad_acpi with fan_control=1")
    })?;

    let user = UserState::load(&cfg.state_file);
    eprintln!(
        "tpfand: mode={} fan={:?} overrides={:?}",
        user.mode.as_deref().unwrap_or("auto"),
        user.fan,
        user.limits
    );
    let default_limits: BTreeMap<_, _> =
        cfg.profiles.iter().map(|(k, p)| (k.clone(), p.limits())).collect();
    let shared = Arc::new((
        Mutex::new(Shared { user, status: Status::default(), dirty: true }),
        Condvar::new(),
    ));
    ipc::serve(Arc::clone(&cfg), Arc::clone(&shared))?;

    let term = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(sig, Arc::clone(&term))?;
    }

    // GPU telemetry on its own thread so a wedged dGPU/NVML call can never
    // stall fan control. Only polls while a dynamic profile is active.
    let gpu_wanted = Arc::new(AtomicBool::new(false));
    let gpu_latest: Arc<Mutex<Option<(GpuSample, Instant)>>> = Arc::new(Mutex::new(None));
    {
        let (wanted, latest) = (Arc::clone(&gpu_wanted), Arc::clone(&gpu_latest));
        std::thread::Builder::new().name("gpu".into()).spawn(move || {
            let mut gpu: Option<Gpu> = None;
            loop {
                if wanted.load(Ordering::Relaxed) {
                    let g = gpu.get_or_insert_with(Gpu::new);
                    if let Some(s) = g.sample() {
                        *latest.lock().unwrap() = Some((s, Instant::now()));
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })?;
    }

    let interval = Duration::from_millis(cfg.interval_ms.max(200));
    let refresh = Duration::from_secs((cfg.watchdog_s as u64 / 3).max(1));
    let mut ctl = Controller::new();
    let mut meter = PowerMeter::new();
    let mut boost = Boost::new(cfg.profile("balanced").1.limits());
    let mut applied_profile = String::new();
    let mut applied_limits: Option<tpfand_proto::Limits> = None;
    let mut logged_boost: Option<(u32, String)> = None;
    let mut last_rapl = Instant::now();
    let mut last_write = Instant::now();
    let mut last_tick = Instant::now();
    let mut written: Option<Fan> = None;
    let mut cpu_watts = None;

    while !term.load(Ordering::Relaxed) {
        let dt = last_tick.elapsed().as_secs_f64();
        let full_tick = dt >= interval.as_secs_f64() * 0.5;
        let dt = dt.max(0.001);
        last_tick = Instant::now();

        let user = {
            let mut sh = shared.0.lock().unwrap();
            sh.dirty = false;
            sh.user.clone()
        };

        // ---- temperatures
        let readings = sensors.read();
        let raw = readings.control;
        if let Some(t) = raw {
            ctl.observe(&cfg, t, readings.peak.unwrap_or(t), dt);
        }
        if full_tick {
            cpu_watts = meter.sample().or(cpu_watts);
        }

        // ---- profile + power limits
        let system = hw::system_profile();
        let wanted = user.mode.clone().unwrap_or_else(|| system.clone());
        let (name, prof) = cfg.profile(&wanted);
        let limits_for = |p: &str| {
            user.limits.get(p).copied().unwrap_or_else(|| cfg.profile(p).1.limits())
        };
        let boost_max = boost::max_limits(prof).map(|d| user.boost_max.unwrap_or(d));
        let profile_changed = name != applied_profile;
        gpu_wanted.store(prof.boost.is_some(), Ordering::Relaxed);
        if profile_changed {
            boost.reset(limits_for(name));
            ctl.release_hold(&cfg);
        }

        let mut boost_info = None;
        let limits = match (&prof.boost, boost_max) {
            (Some(bcfg), Some(max)) => {
                let gpu = gpu_latest
                    .lock()
                    .unwrap()
                    .filter(|(_, t)| t.elapsed() < Duration::from_secs(6))
                    .map(|(s, _)| s);
                let inp = boost::Inputs { cpu_watts, gpu, temp: ctl.ema, critical: ctl.critical };
                let l = boost.step(bcfg, limits_for(name), max, &inp, dt, Instant::now());
                boost_info = Some(boost.info.clone());
                l
            }
            _ => limits_for(name),
        };

        if profile_changed || Some(limits) != applied_limits {
            hw::apply_rapl(limits);
            let log = if prof.boost.is_some() {
                // Only log meaningful boost moves.
                let r = boost.info.reason.clone();
                let big = logged_boost.as_ref().is_none_or(|(w, lr)| {
                    w.abs_diff(limits.pl1_w) >= 10 || *lr != r
                });
                if big {
                    logged_boost = Some((limits.pl1_w, r));
                }
                profile_changed || big
            } else {
                true
            };
            if log {
                eprintln!(
                    "tpfand: profile {name} ({}) PL1={}W PL2={}W{}",
                    if user.mode.is_some() { "pinned" } else { "auto" },
                    limits.pl1_w,
                    limits.pl2_w,
                    boost_info.as_ref().map(|b| format!(" — {}", b.reason)).unwrap_or_default()
                );
            }
            applied_profile = name.to_string();
            applied_limits = Some(limits);
            last_rapl = Instant::now();
        } else if last_rapl.elapsed().as_secs_f64() >= cfg.rapl_recheck_s {
            // Firmware rewrites limits on platform_profile changes; enforce ours.
            if hw::apply_rapl(limits) {
                eprintln!("tpfand: firmware changed power limits; reapplied");
            }
            last_rapl = Instant::now();
        }

        // ---- fan
        let want = if raw.is_none() || ctl.critical {
            Fan::FullSpeed // no sensors or critical: always safe
        } else {
            match user.fan {
                FanMode::Curve => ctl.curve_target(&cfg, prof),
                FanMode::Max => Fan::FullSpeed,
                FanMode::Firmware => Fan::Auto,
            }
        };
        if written != Some(want) || last_write.elapsed() >= refresh {
            match hw::fan_write(&want.cmd()) {
                Err(e) => eprintln!("warn: fan write: {e}"),
                Ok(()) => {
                    if written != Some(want) {
                        eprintln!(
                            "tpfand: {} (control {:.0}°C, peak {:.0}°C, smoothed {:.1}°C{})",
                            want.cmd(),
                            raw.unwrap_or(f64::NAN),
                            readings.peak.unwrap_or(f64::NAN),
                            ctl.ema.unwrap_or(f64::NAN),
                            if ctl.critical { ", CRITICAL" } else { "" }
                        );
                        ctl.last_change = Instant::now();
                    }
                    written = Some(want);
                    ctl.fan = Some(want);
                    last_write = Instant::now();
                }
            }
        }

        // ---- publish status, then wait for next tick or a panel change
        let status = Status {
            mode: user.mode.clone(),
            system_profile: system,
            effective_profile: name.to_string(),
            fan_mode: user.fan,
            fan_level: want.cmd().trim_start_matches("level ").to_string(),
            fan_rpm: sensors.rpm(),
            critical: ctl.critical,
            temps: readings.display,
            smoothed_temp: ctl.ema,
            package_watts: cpu_watts,
            limits: cfg.profiles.keys().map(|k| (k.clone(), limits_for(k))).collect(),
            default_limits: default_limits.clone(),
            applied: hw::read_rapl(),
            can_write: false,
            boost: boost_info,
            boost_max: cfg
                .profiles
                .values()
                .find_map(boost::max_limits)
                .map(|d| user.boost_max.unwrap_or(d)),
            default_boost_max: cfg.profiles.values().find_map(boost::max_limits),
        };
        let (lock, cvar) = &*shared;
        let mut sh = lock.lock().unwrap();
        sh.status = status;
        if !sh.dirty {
            let _ = cvar.wait_timeout(sh, interval).unwrap();
        }
    }

    eprintln!("tpfand: exiting, returning fan to firmware control");
    let _ = hw::fan_write("level auto");
    let _ = hw::fan_write("watchdog 0");
    let _ = std::fs::remove_file(tpfand_proto::SOCKET_PATH);
    Ok(())
}

fn print_status() -> Res<()> {
    let s = Client::connect()
        .map_err(|e| format!("cannot reach daemon at {}: {e}", tpfand_proto::SOCKET_PATH))?
        .status()?;
    println!(
        "mode:     {} (system: {}, applying: {})",
        s.mode.as_deref().unwrap_or("auto"),
        s.system_profile,
        s.effective_profile
    );
    for (l, t) in &s.temps {
        println!("  {l:12} {t:.0}°C");
    }
    if let Some(t) = s.smoothed_temp {
        println!("  {:12} {t:.1}°C", "smoothed");
    }
    println!(
        "fan:      {:?}, level {}, rpm {:?}{}",
        s.fan_mode,
        s.fan_level,
        s.fan_rpm,
        if s.critical { "  ** CRITICAL **" } else { "" }
    );
    if let Some(a) = s.applied {
        println!("limits:   PL1={}W PL2={}W", a.pl1_w, a.pl2_w);
    }
    if let Some(w) = s.package_watts {
        println!("package:  {w:.1}W");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = None;
    let mut cmd = "run";
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--config" => config = it.next().map(|s| s.as_str()),
            "run" | "status" | "check" => cmd = a.as_str(),
            "-h" | "--help" => {
                println!(
                    "usage: tpfand [run|status|check] [-c CONFIG]\n  default config: {}",
                    config::CONFIG_PATH
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let res = match cmd {
        "status" => print_status(),
        _ => match Config::load(config) {
            Err(e) => Err(format!("config error: {e}").into()),
            Ok(cfg) if cmd == "check" => {
                println!("config OK: profiles {:?}", cfg.profiles.keys().collect::<Vec<_>>());
                Ok(())
            }
            Ok(cfg) => run(cfg),
        },
    };
    if let Err(e) = res {
        eprintln!("tpfand: {e}");
        std::process::exit(1);
    }
}
