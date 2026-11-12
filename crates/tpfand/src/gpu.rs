//! NVIDIA dGPU telemetry via NVML (dlopen'd at runtime; optional).

use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub struct GpuSample {
    pub watts: f64,
    pub util: u32,
    pub temp: Option<f64>,
    /// GPU clocks are being cut for thermal reasons (shared heatsink is hot).
    pub thermal_throttled: bool,
    /// GPU is pinned at its power cap.
    pub power_capped: bool,
}

pub struct Gpu {
    nvml: Option<Nvml>,
    next_try: Instant,
    warned: bool,
}

impl Gpu {
    pub fn new() -> Self {
        let mut g = Gpu { nvml: None, next_try: Instant::now(), warned: false };
        g.ensure();
        g
    }

    fn ensure(&mut self) {
        if self.nvml.is_some() || Instant::now() < self.next_try {
            return;
        }
        match Nvml::init() {
            Ok(n) => {
                eprintln!("tpfand: NVML ready, GPU telemetry enabled");
                self.nvml = Some(n);
            }
            Err(e) => {
                if !self.warned {
                    eprintln!("tpfand: NVML unavailable ({e}); GPU treated as idle");
                    self.warned = true;
                }
                self.next_try = Instant::now() + Duration::from_secs(30);
            }
        }
    }

    pub fn sample(&mut self) -> Option<GpuSample> {
        self.ensure();
        let nvml = self.nvml.as_ref()?;
        let res = (|| {
            let dev = nvml.device_by_index(0)?;
            let watts = dev.power_usage()? as f64 / 1000.0;
            let util = dev.utilization_rates().map(|u| u.gpu).unwrap_or(0);
            let temp = dev.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f64);
            let r = dev.current_throttle_reasons().unwrap_or(ThrottleReasons::empty());
            Ok::<_, nvml_wrapper::error::NvmlError>(GpuSample {
                watts,
                util,
                temp,
                thermal_throttled: r.intersects(
                    ThrottleReasons::SW_THERMAL_SLOWDOWN | ThrottleReasons::HW_THERMAL_SLOWDOWN,
                ),
                power_capped: r.contains(ThrottleReasons::SW_POWER_CAP),
            })
        })();
        match res {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("tpfand: NVML read failed ({e}); will retry");
                self.nvml = None;
                self.next_try = Instant::now() + Duration::from_secs(30);
                None
            }
        }
    }
}
