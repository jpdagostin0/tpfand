//! Fan curve controller: smoothing, hysteresis, critical override.

use crate::config::{Config, Profile};
use crate::hw::Fan;
use std::time::{Duration, Instant};

pub struct Controller {
    pub ema: Option<f64>,
    pub fan: Option<Fan>,
    pub last_change: Instant,
    hot_samples: u32,
    pub critical: bool,
}

impl Controller {
    pub fn new() -> Self {
        Controller {
            ema: None,
            fan: None,
            last_change: Instant::now(),
            hot_samples: 0,
            critical: false,
        }
    }

    /// Allow an immediate step down (e.g. after a profile switch).
    pub fn release_hold(&mut self, cfg: &Config) {
        self.last_change = Instant::now() - Duration::from_secs_f64(cfg.min_hold_s + 1.0);
    }

    /// Feed a raw temperature; updates smoothing + critical state.
    pub fn observe(&mut self, cfg: &Config, raw: f64, dt: f64) {
        let ema = match self.ema {
            None => raw,
            Some(prev) => {
                let tau = if raw > prev { cfg.rise_tau_s } else { cfg.fall_tau_s };
                prev + (raw - prev) * (1.0 - (-dt / tau.max(0.01)).exp())
            }
        };
        self.ema = Some(ema);

        if ema >= cfg.critical_temp {
            self.hot_samples += 1;
        } else {
            self.hot_samples = 0;
        }
        if raw >= cfg.critical_instant || self.hot_samples >= cfg.critical_samples {
            self.critical = true;
        } else if self.critical && ema < cfg.critical_release {
            self.critical = false;
        }
    }

    /// Fan level the curve wants given the current state.
    pub fn curve_target(&self, cfg: &Config, prof: &Profile) -> Fan {
        let Some(ema) = self.ema else { return Fan::FullSpeed };
        if self.critical {
            return Fan::FullSpeed;
        }
        let up = Fan::Level(prof.level_for(ema));
        let down = Fan::Level(prof.level_for(ema + cfg.hysteresis));
        match self.fan {
            None | Some(Fan::Auto) => up,
            // Leaving full-speed: settle on the conservative (hysteresis) level.
            Some(Fan::FullSpeed) => down,
            Some(cur) if up.rank() > cur.rank() => up,
            Some(cur)
                if down.rank() < cur.rank()
                    && self.last_change.elapsed().as_secs_f64() >= cfg.min_hold_s =>
            {
                down
            }
            Some(cur) => cur,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse(crate::config::DEFAULT_CONFIG).unwrap()
    }

    #[test]
    fn curve_lookup() {
        let c = cfg();
        let p = &c.profiles["balanced"];
        assert_eq!(p.level_for(30.0), 0);
        assert_eq!(p.level_for(62.0), 2);
        assert_eq!(p.level_for(95.0), 7);
    }

    #[test]
    fn spike_is_smoothed() {
        let c = cfg();
        let p = &c.profiles["balanced"];
        let mut ctl = Controller::new();
        ctl.observe(&c, 50.0, 1.0);
        ctl.fan = Some(Fan::Level(0));
        ctl.observe(&c, 85.0, 1.0);
        let f = ctl.curve_target(&c, p);
        assert!(f.rank() < 3, "{f:?}");
    }

    #[test]
    fn critical_trips_and_releases() {
        let c = cfg();
        let p = &c.profiles["low-power"];
        let mut ctl = Controller::new();
        ctl.observe(&c, 60.0, 1.0);
        for _ in 0..30 {
            ctl.observe(&c, 100.0, 1.0);
        }
        assert_eq!(ctl.curve_target(&c, p), Fan::FullSpeed);
        // Instant trip on extreme temp.
        let mut ctl2 = Controller::new();
        ctl2.observe(&c, 60.0, 1.0);
        ctl2.observe(&c, c.critical_instant, 1.0);
        assert_eq!(ctl2.curve_target(&c, p), Fan::FullSpeed);
        // Cooling down releases.
        ctl.fan = Some(Fan::FullSpeed);
        for _ in 0..120 {
            ctl.observe(&c, 60.0, 1.0);
        }
        assert!(!ctl.critical);
        assert_ne!(ctl.curve_target(&c, p), Fan::FullSpeed);
    }

    #[test]
    fn single_core_spikes_dont_trip_critical() {
        // Observed on a 155H: package temp flips 50 <-> 100°C every ~2 s
        // under a single-threaded load at ~20 W.
        let c = cfg();
        let mut ctl = Controller::new();
        ctl.observe(&c, 55.0, 1.0);
        for i in 0..120 {
            let t = if (i / 2) % 2 == 0 { 100.0 } else { 52.0 };
            ctl.observe(&c, t, 1.0);
            assert!(!ctl.critical, "tripped at sample {i}, ema {:?}", ctl.ema);
        }
    }

    #[test]
    fn hysteresis_holds_level() {
        let c = cfg();
        let p = &c.profiles["balanced"];
        let mut ctl = Controller::new();
        ctl.ema = Some(71.0);
        ctl.fan = Some(Fan::Level(3));
        ctl.release_hold(&c);
        ctl.observe(&c, 60.0, 1.0); // ema ~70, below the 70°C step but within hysteresis
        assert_eq!(ctl.curve_target(&c, p), Fan::Level(3));
    }

    #[test]
    fn steps_down_after_hold() {
        let c = cfg();
        let p = &c.profiles["balanced"];
        let mut ctl = Controller::new();
        ctl.ema = Some(55.0);
        ctl.fan = Some(Fan::Level(4));
        ctl.release_hold(&c);
        ctl.observe(&c, 55.0, 1.0);
        assert_eq!(ctl.curve_target(&c, p), Fan::Level(1));
        ctl.last_change = Instant::now();
        ctl.fan = Some(Fan::Level(4));
        assert_eq!(ctl.curve_target(&c, p), Fan::Level(4));
    }
}
