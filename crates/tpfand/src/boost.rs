//! "Dynamic" power mode: our own take on Dynamic Boost.
//!
//! Moves CPU package power limits within [floor, max] based on load and
//! thermals:
//!  * CPU is power-limited (package draw near PL1) and there's thermal
//!    headroom -> step PL1 up.
//!  * Too hot -> step down quickly.
//!  * Load gone for a while -> decay back to the base limits (quiet).
//!  * dGPU busy -> CPU gets at most `total_budget_w - gpu draw`, and if the
//!    GPU is thermally throttling (shared heatsink) the CPU is cut further so
//!    the GPU keeps its clocks.
//!
//! The GPU's own power cap can't be raised from userspace on laptop parts,
//! so "boost" here means re-balancing the shared thermal budget.

use crate::config::{Boost as BoostCfg, Profile};
use crate::gpu::GpuSample;
use std::time::{Duration, Instant};
use tpfand_proto::{BoostInfo, Limits};

pub struct Inputs {
    pub cpu_watts: Option<f64>,
    pub gpu: Option<GpuSample>,
    /// Smoothed control temperature (same signal the fan curve uses).
    pub temp: Option<f64>,
    pub critical: bool,
}

pub struct Boost {
    pl1: f64,
    /// PL2 is trimmed when hot: the turbo burst is what overshoots first.
    pl2: f64,
    cpu_ema: Option<f64>,
    gpu_ema: Option<f64>,
    last_act: Option<Instant>,
    light_since: Option<Instant>,
    pub info: BoostInfo,
}

impl Boost {
    pub fn new(base: Limits) -> Self {
        Boost {
            pl1: base.pl1_w as f64,
            pl2: base.pl2_w as f64,
            cpu_ema: None,
            gpu_ema: None,
            last_act: None,
            light_since: None,
            info: BoostInfo::default(),
        }
    }

    pub fn reset(&mut self, base: Limits) {
        *self = Boost::new(base);
    }

    fn ema(prev: &mut Option<f64>, v: f64, dt: f64, tau: f64) -> f64 {
        let n = match *prev {
            None => v,
            Some(p) => p + (v - p) * (1.0 - (-dt / tau).exp()),
        };
        *prev = Some(n);
        n
    }

    /// `base` = the profile's (possibly user-overridden) base limits,
    /// `max` = (possibly user-overridden) boost ceiling.
    pub fn step(
        &mut self,
        cfg: &BoostCfg,
        base: Limits,
        max: Limits,
        inp: &Inputs,
        dt: f64,
        now: Instant,
    ) -> Limits {
        let cpu = inp.cpu_watts.map(|w| Self::ema(&mut self.cpu_ema, w, dt, 2.0));
        let gpu_w = inp.gpu.map(|g| Self::ema(&mut self.gpu_ema, g.watts, dt, 3.0)).unwrap_or(0.0);
        // Busy = drawing real power AND doing work. (Laptop RTX parts can sit
        // at ~18 W with 0% util, and NVML's thermal-slowdown flag follows the
        // platform T.Limit, which is set whenever the shared heatsink is hot.)
        let gpu_busy = gpu_w >= cfg.gpu_active_w
            && inp.gpu.is_some_and(|g| g.util as f64 >= cfg.gpu_active_util);
        let gpu_throttled = inp.gpu.is_some_and(|g| g.thermal_throttled) && gpu_busy;

        // Ceiling from the shared budget.
        let mut ceiling = max.pl1_w as f64;
        if gpu_busy {
            ceiling = ceiling.min(cfg.total_budget_w - gpu_w);
        }
        let floor = cfg.min_pl1_w.min(base.pl1_w as f64);
        let ceiling = ceiling.max(floor);

        let act = self
            .last_act
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs_f64(cfg.interval_s));
        if act {
            self.last_act = Some(now);
            let temp = inp.temp.unwrap_or(0.0);
            let limited = cpu.is_some_and(|w| w >= cfg.busy_ratio * self.pl1);
            let light = cpu.is_none_or(|w| w < cfg.light_ratio * self.pl1);
            if light {
                self.light_since.get_or_insert(now);
            } else {
                self.light_since = None;
            }
            let light_long = self
                .light_since
                .is_some_and(|t| now.duration_since(t) >= Duration::from_secs_f64(cfg.decay_after_s));

            let reason;
            let hot = temp >= cfg.target_temp + cfg.temp_band;
            if inp.critical {
                self.pl1 = self.pl1.min(base.pl1_w as f64);
                self.pl2 = self.pl1;
                reason = "critical temperature";
            } else if hot {
                self.pl1 -= cfg.step_down_w;
                self.pl2 -= 2.0 * cfg.step_down_w;
                reason = "too hot, backing off";
            } else if gpu_throttled {
                self.pl1 -= cfg.step_down_w;
                reason = "GPU thermal-throttling, giving it headroom";
            } else if limited && temp < cfg.target_temp {
                // Proportional: big steps when cool, small ones near target
                // (damps the boost/back-off sawtooth).
                let step = cfg.step_up_w * ((cfg.target_temp - temp) / 6.0).clamp(0.25, 1.0);
                self.pl1 += step;
                self.pl2 += step;
                reason = "CPU power-limited, boosting";
            } else if light_long && self.pl1 > base.pl1_w as f64 {
                self.pl1 = (self.pl1 - cfg.step_down_w).max(base.pl1_w as f64);
                reason = "load gone, returning to base";
            } else if limited {
                reason = "holding at thermal target";
            } else if self.pl1 > base.pl1_w as f64 {
                reason = "holding boost";
            } else {
                reason = "base";
            }
            if !inp.critical && !limited && temp < cfg.target_temp {
                // Cool again: let the turbo burst budget recover.
                self.pl2 += cfg.step_up_w;
            }
            self.info.reason = if gpu_busy && self.pl1 >= ceiling {
                format!("{reason}; capped by GPU share ({gpu_w:.0} W)")
            } else {
                reason.to_string()
            };
        }
        self.pl1 = self.pl1.clamp(floor, ceiling);

        let pl1 = self.pl1.round() as u32;
        // PL2: at most the base turbo budget (or PL1 if boosted above it),
        // within the ceiling (+ some burst room when sharing with the GPU),
        // never below PL1.
        let pl2_cap = if gpu_busy {
            (ceiling + cfg.gpu_burst_w).min(max.pl2_w as f64)
        } else {
            max.pl2_w as f64
        };
        let pl2_hi = (base.pl2_w as f64).max(self.pl1).min(pl2_cap).max(self.pl1);
        self.pl2 = self.pl2.clamp(self.pl1, pl2_hi);
        let pl2 = (self.pl2.round() as u32).max(pl1);

        self.info.cpu_watts = cpu;
        self.info.gpu_watts = inp.gpu.map(|_| gpu_w);
        self.info.gpu_util = inp.gpu.map(|g| g.util);
        self.info.gpu_temp = inp.gpu.and_then(|g| g.temp);
        self.info.gpu_thermal_throttled = gpu_throttled;
        self.info.gpu_power_capped = gpu_busy && inp.gpu.is_some_and(|g| g.power_capped);
        self.info.ceiling_w = ceiling.round() as u32;
        Limits { pl1_w: pl1, pl2_w: pl2 }
    }
}

/// Defaults a dynamic profile falls back to when no ceiling override exists.
pub fn max_limits(p: &Profile) -> Option<Limits> {
    p.boost.as_ref().map(|b| Limits { pl1_w: b.max_pl1_w, pl2_w: b.max_pl2_w })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_CONFIG};

    fn setup() -> (BoostCfg, Limits, Limits) {
        let c = Config::parse(DEFAULT_CONFIG).unwrap();
        let p = &c.profiles["dynamic"];
        (p.boost.clone().unwrap(), p.limits(), max_limits(p).unwrap())
    }

    fn run(b: &mut Boost, cfg: &BoostCfg, base: Limits, max: Limits, secs: u32, f: impl Fn(&Boost) -> Inputs) -> (Limits, Instant) {
        let mut now = Instant::now();
        let mut l = base;
        for _ in 0..secs {
            now += Duration::from_secs(1);
            let inp = f(b);
            l = b.step(cfg, base, max, &inp, 1.0, now);
        }
        (l, now)
    }

    #[test]
    fn boosts_under_cpu_load_with_headroom() {
        let (cfg, base, max) = setup();
        let mut b = Boost::new(base);
        // CPU always pegged at its limit, cool enough.
        let (l, _) = run(&mut b, &cfg, base, max, 120, |b| Inputs {
            cpu_watts: Some(b.pl1),
            gpu: None,
            temp: Some(75.0),
            critical: false,
        });
        assert_eq!(l.pl1_w, max.pl1_w, "{l:?}");
    }

    #[test]
    fn backs_off_when_hot_and_decays_when_idle() {
        let (cfg, base, max) = setup();
        let mut b = Boost::new(base);
        b.pl1 = 90.0;
        let (l, _) = run(&mut b, &cfg, base, max, 10, |b| Inputs {
            cpu_watts: Some(b.pl1),
            gpu: None,
            temp: Some(cfg.target_temp + cfg.temp_band + 1.0),
            critical: false,
        });
        assert!(l.pl1_w < 90, "{l:?}");
        assert!(l.pl2_w < base.pl2_w, "turbo budget should shrink when hot: {l:?}");
        let (l, _) = run(&mut b, &cfg, base, max, 120, |_| Inputs {
            cpu_watts: Some(5.0),
            gpu: None,
            temp: Some(55.0),
            critical: false,
        });
        assert_eq!(l.pl1_w, base.pl1_w);
    }

    #[test]
    fn yields_to_busy_gpu() {
        let (cfg, base, max) = setup();
        let mut b = Boost::new(base);
        let gpu = GpuSample { watts: 60.0, util: 99, temp: Some(80.0), ..Default::default() };
        // An idle-but-awake GPU (18 W, 0% util, flags set) must not squeeze the CPU.
        let idle_gpu = GpuSample { watts: 18.0, util: 0, thermal_throttled: true, ..Default::default() };
        let mut b2 = Boost::new(base);
        let (l2, _) = run(&mut b2, &cfg, base, max, 120, |b| Inputs {
            cpu_watts: Some(b.pl1),
            gpu: Some(idle_gpu),
            temp: Some(75.0),
            critical: false,
        });
        assert_eq!(l2.pl1_w, max.pl1_w, "{l2:?}");
        let (l, _) = run(&mut b, &cfg, base, max, 120, |b| Inputs {
            cpu_watts: Some(b.pl1),
            gpu: Some(gpu),
            temp: Some(75.0),
            critical: false,
        });
        assert!(l.pl1_w as f64 <= cfg.total_budget_w - 60.0 + 0.5, "{l:?}");
        assert!(l.pl2_w >= l.pl1_w);
    }
}
