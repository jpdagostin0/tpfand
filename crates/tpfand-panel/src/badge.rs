//! Tray "badge": control temperature + package watts drawn as text, coloured
//! green / yellow / red like Core Temp's readouts.
//!
//! GNOME's AppIndicator extension squashes pixmap icons into a square, but an
//! absolute file path as the icon name is loaded at its own aspect ratio when
//! it is at least 1.5× wider than tall (the indicator-multiload path). An SVG
//! is rasterised at the display scale, so the text stays crisp on HiDPI.
//! The extension caches icons by name, so every new image gets a new file.

use std::path::PathBuf;
use tpfand_proto::Status;

/// Logical size (the shell multiplies by its scale factor).
const W: u32 = 70;
const H: u32 = 16;

const GREEN: &str = "#73d216";
const YELLOW: &str = "#edd400";
const RED: &str = "#ef2929";
const GREY: &str = "#9a9996";

/// Temperature colour (°C). Core Ultra TjMax is 110°C; tpfand's critical
/// release point is 85°C.
fn temp_color(t: f64, critical: bool) -> &'static str {
    if critical || t >= 85.0 {
        RED
    } else if t >= 70.0 {
        YELLOW
    } else {
        GREEN
    }
}

/// Watts colour relative to the PL1 currently programmed: under ~80% of the
/// sustained limit is green, at the limit yellow, above it (PL2 burst) red.
fn watts_color(w: f64, pl1: Option<u32>) -> &'static str {
    let pl1 = pl1.map(f64::from).unwrap_or(45.0);
    if w > pl1 + 2.0 {
        RED
    } else if w >= pl1 * 0.8 {
        YELLOW
    } else {
        GREEN
    }
}

/// The temperature the fan curve uses: max(CPU core average, GPU). Falls back
/// to the smoothed control temp, then to the hottest reading.
pub fn control_temp(s: &Status) -> Option<f64> {
    let pick = |name: &str| s.temps.iter().find(|(l, _)| l == name).map(|(_, t)| *t);
    match (pick("CPU core average"), pick("GPU")) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => s
            .smoothed_temp
            .or_else(|| s.temps.iter().map(|(_, t)| *t).reduce(f64::max)),
    }
}

pub fn svg(s: &Status) -> String {
    let (temp, tc) = match control_temp(s) {
        Some(t) => (format!("{:.0}°", t), temp_color(t, s.critical)),
        None => ("--°".into(), GREY),
    };
    let (watts, wc) = match s.package_watts {
        Some(w) => (format!("{:.0}W", w), watts_color(w, s.applied.map(|a| a.pl1_w))),
        None => ("--W".into(), GREY),
    };
    // Right-aligned columns at fixed positions so the icon doesn't jitter as
    // digit counts change ("100°" / "140W" still fit).
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}"><g font-family="Noto Sans, Cantarell, sans-serif" font-weight="bold" font-size="13" text-anchor="end"><text x="30" y="12.5" fill="{tc}">{temp}</text><text x="{wx}" y="12.5" fill="{wc}">{watts}</text></g></svg>"#,
        wx = W - 1,
    )
}

/// Writes badge images to a per-user runtime dir, one new file per change.
pub struct Badge {
    dir: Option<PathBuf>,
    last_svg: String,
    path: Option<PathBuf>,
    serial: u64,
}

impl Badge {
    pub fn new() -> Self {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
        let dir = base.map(|b| b.join("tpfand-panel")).filter(|d| {
            // Drop images left behind by a previous (crashed) instance.
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).is_ok()
        });
        Badge { dir, last_svg: String::new(), path: None, serial: 0 }
    }

    /// Current image path, if any.
    pub fn path(&self) -> Option<String> {
        self.path.as_ref().map(|p| p.to_string_lossy().into_owned())
    }

    /// Re-render for `status` (`None` = no data: fall back to a themed icon).
    pub fn update(&mut self, status: Option<&Status>) {
        let Some(s) = status else {
            self.clear();
            return;
        };
        let Some(dir) = &self.dir else { return };
        let svg = svg(s);
        if svg == self.last_svg && self.path.is_some() {
            return;
        }
        self.serial += 1;
        let path = dir.join(format!("badge-{}-{}.svg", std::process::id(), self.serial));
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &svg).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // The shell has long finished loading the previous image (we update
        // every couple of seconds), so it's safe to remove.
        if let Some(old) = self.path.replace(path) {
            let _ = std::fs::remove_file(old);
        }
        self.last_svg = svg;
    }

    pub fn clear(&mut self) {
        if let Some(old) = self.path.take() {
            let _ = std::fs::remove_file(old);
        }
        self.last_svg.clear();
    }
}

impl Drop for Badge {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpfand_proto::Limits;

    fn status(temps: &[(&str, f64)], watts: Option<f64>) -> Status {
        Status {
            temps: temps.iter().map(|(l, t)| (l.to_string(), *t)).collect(),
            package_watts: watts,
            applied: Some(Limits { pl1_w: 45, pl2_w: 90 }),
            ..Default::default()
        }
    }

    #[test]
    fn uses_control_temp_not_hottest_core() {
        let s = status(
            &[("CPU core average", 55.0), ("CPU hottest core", 100.0), ("GPU", 48.0)],
            Some(20.0),
        );
        assert_eq!(control_temp(&s), Some(55.0));
        let svg = svg(&s);
        assert!(svg.contains(&format!("fill=\"{GREEN}\">55°")), "{svg}");
        assert!(svg.contains(&format!("fill=\"{GREEN}\">20W")), "{svg}");
    }

    #[test]
    fn colours() {
        assert_eq!(temp_color(69.9, false), GREEN);
        assert_eq!(temp_color(70.0, false), YELLOW);
        assert_eq!(temp_color(85.0, false), RED);
        assert_eq!(temp_color(50.0, true), RED);
        assert_eq!(watts_color(30.0, Some(45)), GREEN);
        assert_eq!(watts_color(44.0, Some(45)), YELLOW);
        assert_eq!(watts_color(80.0, Some(45)), RED);
    }

    #[test]
    fn missing_data_is_grey() {
        let svg = svg(&status(&[], None));
        assert!(svg.contains(&format!("fill=\"{GREY}\">--°")));
        assert!(svg.contains(&format!("fill=\"{GREY}\">--W")));
    }

    #[test]
    fn wide_enough_for_custom_image_path() {
        assert!(W as f64 >= H as f64 * 1.5);
    }
}
