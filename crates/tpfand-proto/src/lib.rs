//! Wire protocol between `tpfand` (root daemon) and its clients.
//!
//! Transport: Unix stream socket at [`SOCKET_PATH`]. Each request is one line
//! of JSON ([`Request`]); the daemon answers with one line of JSON
//! ([`Response`]). A connection may carry any number of request/response pairs.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SOCKET_PATH: &str = "/run/tpfand.sock";

/// Profile names, matching /sys/firmware/acpi/platform_profile.
pub const PROFILES: &[&str] = &["low-power", "balanced", "performance"];

pub fn profile_label(p: &str) -> &str {
    match p {
        "low-power" => "Power Saver",
        "balanced" => "Balanced",
        "performance" => "Performance",
        other => other,
    }
}

/// How the fan is driven.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FanMode {
    /// tpfand's per-profile curve (default).
    #[default]
    Curve,
    /// Always full-speed.
    Max,
    /// Hand the fan back to the embedded controller's built-in curve.
    Firmware,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub pl1_w: u32,
    pub pl2_w: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    /// `profile: None` = auto (follow the system power mode).
    SetMode { profile: Option<String> },
    SetFan { fan: FanMode },
    /// Override power limits for a profile; `limits: None` resets to config.
    SetLimits { profile: String, limits: Option<Limits> },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Status(Box<Status>),
    Error { message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Status {
    /// `None` = auto.
    pub mode: Option<String>,
    /// Profile reported by the system (power-profiles-daemon / firmware).
    pub system_profile: String,
    /// Profile tpfand is actually applying.
    pub effective_profile: String,
    pub fan_mode: FanMode,
    /// Last command written to the EC, e.g. "level 3" / "level full-speed".
    pub fan_level: String,
    pub fan_rpm: Vec<u32>,
    pub critical: bool,
    pub temps: Vec<(String, f64)>,
    pub smoothed_temp: Option<f64>,
    /// Package power draw from RAPL energy counters.
    pub package_watts: Option<f64>,
    /// Effective limits per profile (config + overrides).
    pub limits: BTreeMap<String, Limits>,
    /// Config defaults per profile.
    pub default_limits: BTreeMap<String, Limits>,
    /// Limits currently programmed into the CPU.
    pub applied: Option<Limits>,
    /// Whether the connected client may change settings.
    pub can_write: bool,
}

/// Blocking client.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect() -> std::io::Result<Self> {
        let stream = UnixStream::connect(SOCKET_PATH)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client { stream, reader })
    }

    pub fn call(&mut self, req: &Request) -> std::io::Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        let mut buf = String::new();
        if self.reader.read_line(&mut buf)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed connection",
            ));
        }
        Ok(serde_json::from_str(&buf)?)
    }

    pub fn status(&mut self) -> std::io::Result<Status> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(*s),
            Response::Error { message } => Err(std::io::Error::other(message)),
            Response::Ok => Err(std::io::Error::other("unexpected response")),
        }
    }

    /// Send a command; maps `Response::Error` into an io error.
    pub fn command(&mut self, req: &Request) -> std::io::Result<()> {
        match self.call(req)? {
            Response::Error { message } => Err(std::io::Error::other(message)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = Request::SetLimits {
            profile: "balanced".into(),
            limits: Some(Limits { pl1_w: 40, pl2_w: 70 }),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"cmd\":\"set-limits\""));
        let _: Request = serde_json::from_str(&s).unwrap();
        let s = serde_json::to_string(&Request::SetMode { profile: None }).unwrap();
        let _: Request = serde_json::from_str(&s).unwrap();
        let s = serde_json::to_string(&Response::Status(Box::default())).unwrap();
        let _: Response = serde_json::from_str(&s).unwrap();
    }
}
