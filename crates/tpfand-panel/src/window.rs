//! egui control window: status, mode, fan mode, per-profile power limits.

use eframe::egui;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tpfand_proto::{Client, FanMode, Limits, PROFILES, Request, Status, profile_label};

const LIMIT_MIN: u32 = 5;
const LIMIT_MAX: u32 = 140;

pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("tpfand")
            .with_app_id("tpfand-panel")
            .with_inner_size([440.0, 620.0])
            .with_min_inner_size([380.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("tpfand", options, Box::new(|_cc| Ok(Box::new(Panel::new()))))
        .map_err(|e| e.to_string())
}

struct Panel {
    client: Option<Client>,
    status: Option<Status>,
    error: Option<String>,
    last_poll: Option<Instant>,
    /// Slider values being edited, per profile.
    edits: BTreeMap<String, Limits>,
}

impl Panel {
    fn new() -> Self {
        Panel { client: None, status: None, error: None, last_poll: None, edits: BTreeMap::new() }
    }

    fn with_client<T>(&mut self, f: impl FnOnce(&mut Client) -> std::io::Result<T>) -> Option<T> {
        if self.client.is_none() {
            match Client::connect() {
                Ok(c) => self.client = Some(c),
                Err(e) => {
                    self.error = Some(format!("cannot reach tpfand: {e}"));
                    return None;
                }
            }
        }
        match f(self.client.as_mut().unwrap()) {
            Ok(v) => Some(v),
            Err(e) => {
                self.error = Some(e.to_string());
                self.client = None;
                None
            }
        }
    }

    fn poll(&mut self) {
        if let Some(s) = self.with_client(|c| c.status()) {
            for (p, l) in &s.limits {
                self.edits.entry(p.clone()).or_insert(*l);
            }
            self.status = Some(s);
            self.error = None;
        }
        self.last_poll = Some(Instant::now());
    }

    fn send(&mut self, req: Request) {
        if self.with_client(|c| c.command(&req)).is_some() {
            self.error = None;
        }
        self.poll();
    }
}

impl eframe::App for Panel {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.poll();
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.contents(ui));
        });
    }
}

impl Panel {
    fn contents(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        let Some(s) = self.status.clone() else {
            ui.label("Waiting for tpfand…  (is tpfand.service running?)");
            return;
        };
        if !s.can_write {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Read-only: your user isn't allowed to change settings (see allow_group).",
            );
        }

        // ------------------------------------------------------- status
        ui.heading("Status");
        egui::Grid::new("status").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (label, t) in &s.temps {
                ui.label(label);
                ui.label(format!("{t:.0} °C"));
                ui.end_row();
            }
            if let Some(t) = s.smoothed_temp {
                ui.label("Fan control temp");
                ui.label(format!("{t:.1} °C"));
                ui.end_row();
            }
            ui.label("Fan");
            let rpm: Vec<_> = s.fan_rpm.iter().map(|r| r.to_string()).collect();
            let txt = format!("level {} · {} rpm", s.fan_level, rpm.join(" / "));
            if s.critical {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("{txt}  — CRITICAL"));
            } else {
                ui.label(txt);
            }
            ui.end_row();
            if let Some(w) = s.package_watts {
                ui.label("CPU package power");
                ui.label(format!("{w:.1} W"));
                ui.end_row();
            }
            if let Some(a) = s.applied {
                ui.label("Applied limits");
                ui.label(format!("PL1 {} W · PL2 {} W", a.pl1_w, a.pl2_w));
                ui.end_row();
            }
        });

        ui.separator();

        // --------------------------------------------------------- mode
        ui.heading("Thermal profile");
        ui.add_enabled_ui(s.can_write, |ui| {
            let mut sel = s.mode.clone();
            ui.radio_value(
                &mut sel,
                None,
                format!("Auto — follow system power mode ({})", profile_label(&s.system_profile)),
            );
            for p in PROFILES {
                ui.radio_value(&mut sel, Some(p.to_string()), profile_label(p));
            }
            if sel != s.mode {
                self.send(Request::SetMode { profile: sel });
            }
        });
        ui.label(
            egui::RichText::new(format!("Applying: {}", profile_label(&s.effective_profile)))
                .weak(),
        );

        ui.separator();

        // ---------------------------------------------------------- fan
        ui.heading("Fan");
        ui.add_enabled_ui(s.can_write, |ui| {
            let mut fan = s.fan_mode;
            ui.horizontal(|ui| {
                ui.radio_value(&mut fan, FanMode::Curve, "Profile curve");
                ui.radio_value(&mut fan, FanMode::Max, "Maximum");
                ui.radio_value(&mut fan, FanMode::Firmware, "Firmware auto");
            });
            if fan != s.fan_mode {
                self.send(Request::SetFan { fan });
            }
        });
        ui.label(
            egui::RichText::new("Critical temperatures always force full speed.").weak(),
        );

        ui.separator();

        // ------------------------------------------------------- limits
        ui.heading("Power limits (TDP)");
        ui.label(
            egui::RichText::new(
                "PL1 = sustained package power, PL2 = short turbo bursts.",
            )
            .weak(),
        );
        for p in PROFILES {
            let Some(current) = s.limits.get(*p).copied() else { continue };
            let default = s.default_limits.get(*p).copied();
            let mut edit = *self.edits.get(*p).unwrap_or(&current);
            let active = s.effective_profile == *p;

            ui.add_space(6.0);
            let title = if active {
                format!("{} (active)", profile_label(p))
            } else {
                profile_label(p).to_string()
            };
            ui.label(egui::RichText::new(title).strong());
            ui.add_enabled_ui(s.can_write, |ui| {
                ui.add(egui::Slider::new(&mut edit.pl1_w, LIMIT_MIN..=LIMIT_MAX).text("PL1 W"));
                ui.add(egui::Slider::new(&mut edit.pl2_w, LIMIT_MIN..=LIMIT_MAX).text("PL2 W"));
                if edit.pl2_w < edit.pl1_w {
                    edit.pl2_w = edit.pl1_w;
                }
                ui.horizontal(|ui| {
                    let changed = edit != current;
                    if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                        self.send(Request::SetLimits {
                            profile: p.to_string(),
                            limits: Some(edit),
                        });
                    }
                    let is_default = Some(current) == default;
                    if ui.add_enabled(!is_default, egui::Button::new("Reset to default")).clicked() {
                        self.send(Request::SetLimits { profile: p.to_string(), limits: None });
                        if let Some(d) = default {
                            edit = d;
                        }
                    }
                    if changed && ui.button("Revert").clicked() {
                        edit = current;
                    }
                    if let Some(d) = default {
                        ui.label(
                            egui::RichText::new(format!("default {}/{} W", d.pl1_w, d.pl2_w))
                                .weak(),
                        );
                    }
                });
            });
            self.edits.insert(p.to_string(), edit);
        }
    }
}
