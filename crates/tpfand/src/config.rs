use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use tpfand_proto::Limits;

pub const DEFAULT_CONFIG: &str = include_str!("../tpfand.toml");
pub const CONFIG_PATH: &str = "/etc/tpfand.toml";

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub interval_ms: u64,
    pub rise_tau_s: f64,
    pub fall_tau_s: f64,
    pub hysteresis: f64,
    pub min_hold_s: f64,
    pub critical_temp: f64,
    #[serde(default = "d_instant")]
    pub critical_instant: f64,
    pub critical_release: f64,
    pub critical_samples: u32,
    pub watchdog_s: u32,
    pub rapl_recheck_s: f64,
    #[serde(default = "d_min")]
    pub limit_min_w: u32,
    #[serde(default = "d_max")]
    pub limit_max_w: u32,
    #[serde(default = "d_group")]
    pub allow_group: String,
    #[serde(default = "d_state")]
    pub state_file: String,
    pub profiles: BTreeMap<String, Profile>,
}

fn d_util() -> f64 {
    30.0
}
fn d_instant() -> f64 {
    106.0
}
fn d_min() -> u32 {
    5
}
fn d_max() -> u32 {
    140
}
fn d_group() -> String {
    "sudo".into()
}
fn d_state() -> String {
    "/var/lib/tpfand/state.json".into()
}

#[derive(Deserialize, Debug, Clone)]
pub struct Profile {
    pub pl1_w: u32,
    pub pl2_w: u32,
    pub curve: Vec<(f64, u8)>,
    /// Present => this is a dynamic-boost profile; pl1_w/pl2_w are the base.
    pub boost: Option<Boost>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Boost {
    pub max_pl1_w: u32,
    pub max_pl2_w: u32,
    pub min_pl1_w: f64,
    pub target_temp: f64,
    pub temp_band: f64,
    pub busy_ratio: f64,
    pub light_ratio: f64,
    pub decay_after_s: f64,
    pub step_up_w: f64,
    pub step_down_w: f64,
    pub interval_s: f64,
    pub total_budget_w: f64,
    pub gpu_active_w: f64,
    #[serde(default = "d_util")]
    pub gpu_active_util: f64,
    pub gpu_burst_w: f64,
}

impl Profile {
    pub fn level_for(&self, t: f64) -> u8 {
        self.curve
            .iter()
            .filter(|(temp, _)| t >= *temp)
            .map(|&(_, lvl)| lvl)
            .max()
            .unwrap_or(0)
    }
    pub fn limits(&self) -> Limits {
        Limits { pl1_w: self.pl1_w, pl2_w: self.pl2_w }
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Res<Config> {
        let text = match path {
            Some(p) => fs::read_to_string(p)?,
            None => fs::read_to_string(CONFIG_PATH).unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Res<Config> {
        let mut cfg: Config = toml::from_str(text)?;
        if let Some(p) = cfg.profiles.remove("power-saver") {
            cfg.profiles.entry("low-power".into()).or_insert(p);
        }
        for (name, p) in cfg.profiles.iter_mut() {
            if p.curve.is_empty() {
                return Err(format!("profile {name}: empty curve").into());
            }
            if let Some(&(_, l)) = p.curve.iter().find(|(_, l)| *l > 7) {
                return Err(format!("profile {name}: fan level {l} > 7").into());
            }
            p.curve.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        if !cfg.profiles.contains_key("balanced") {
            return Err("config must define [profiles.balanced] (used as fallback)".into());
        }
        if cfg.limit_min_w == 0 || cfg.limit_min_w > cfg.limit_max_w {
            return Err("invalid limit_min_w/limit_max_w".into());
        }
        Ok(cfg)
    }

    pub fn profile(&self, name: &str) -> (&str, &Profile) {
        match self.profiles.get_key_value(name) {
            Some((k, p)) => (k.as_str(), p),
            None => {
                let (k, p) = self.profiles.get_key_value("balanced").unwrap();
                (k.as_str(), p)
            }
        }
    }

    /// Clamp user-provided limits into the allowed range, PL2 >= PL1.
    pub fn clamp(&self, l: Limits) -> Limits {
        let pl1 = l.pl1_w.clamp(self.limit_min_w, self.limit_max_w);
        let pl2 = l.pl2_w.clamp(self.limit_min_w, self.limit_max_w).max(pl1);
        Limits { pl1_w: pl1, pl2_w: pl2 }
    }
}
