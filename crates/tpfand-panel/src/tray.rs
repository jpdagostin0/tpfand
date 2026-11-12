//! Tray icon: quick mode / fan switching, opens the egui window.

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem};
use std::process::{Child, Command};
use std::time::Duration;
use tpfand_proto::{Client, FanMode, PROFILES, Request, Status, profile_label};

struct Tray {
    status: Option<Status>,
    error: Option<String>,
    window: Option<Child>,
    quit: bool,
}

impl Tray {
    fn send(&mut self, req: Request) {
        let res = Client::connect().and_then(|mut c| {
            c.command(&req)?;
            c.status()
        });
        match res {
            Ok(s) => {
                self.status = Some(s);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn open_window(&mut self) {
        if let Some(child) = &mut self.window {
            if matches!(child.try_wait(), Ok(None)) {
                return; // already open
            }
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| "tpfand-panel".into());
        match Command::new(exe).arg("--window").spawn() {
            Ok(c) => self.window = Some(c),
            Err(e) => self.error = Some(format!("cannot open window: {e}")),
        }
    }

    fn summary(&self) -> String {
        match (&self.status, &self.error) {
            (_, Some(e)) => format!("tpfand unavailable: {e}"),
            (None, None) => "connecting…".into(),
            (Some(s), None) => {
                let t = s.temps.iter().map(|(_, t)| *t).fold(f64::NAN, f64::max);
                let rpm = s.fan_rpm.iter().max().copied().unwrap_or(0);
                let w = s.package_watts.map(|w| format!(" · {w:.0} W")).unwrap_or_default();
                format!("{t:.0}°C · fan {} ({rpm} rpm){w}", s.fan_level)
            }
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "tpfand-panel".into()
    }

    fn title(&self) -> String {
        "tpfand".into()
    }

    fn icon_name(&self) -> String {
        match &self.status {
            None => "dialog-warning-symbolic",
            Some(s) if s.critical => "dialog-warning-symbolic",
            Some(s) => match s.effective_profile.as_str() {
                "low-power" => "power-profile-power-saver-symbolic",
                "performance" => "power-profile-performance-symbolic",
                _ => "power-profile-balanced-symbolic",
            },
        }
        .into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "tpfand".into(),
            description: self.summary(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_window();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem { label: self.summary(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
        ];

        let (mode, system, fan) = match &self.status {
            Some(s) => (s.mode.clone(), s.system_profile.clone(), s.fan_mode),
            None => (None, String::new(), FanMode::Curve),
        };
        let mut options = vec![RadioItem {
            label: if system.is_empty() {
                "Auto".into()
            } else {
                format!("Auto (system: {})", profile_label(&system))
            },
            ..Default::default()
        }];
        options.extend(PROFILES.iter().map(|p| RadioItem {
            label: profile_label(p).into(),
            ..Default::default()
        }));
        let selected = match &mode {
            None => 0,
            Some(m) => PROFILES.iter().position(|p| p == m).map_or(0, |i| i + 1),
        };
        items.push(
            RadioGroup {
                selected,
                select: Box::new(|this: &mut Self, i| {
                    let profile = (i > 0).then(|| PROFILES[i - 1].to_string());
                    this.send(Request::SetMode { profile });
                }),
                options,
            }
            .into(),
        );
        items.push(MenuItem::Separator);

        const FANS: [(FanMode, &str); 3] = [
            (FanMode::Curve, "Fan: tpfand curve"),
            (FanMode::Max, "Fan: maximum"),
            (FanMode::Firmware, "Fan: firmware auto"),
        ];
        items.push(
            RadioGroup {
                selected: FANS.iter().position(|(f, _)| *f == fan).unwrap_or(0),
                select: Box::new(|this: &mut Self, i| {
                    this.send(Request::SetFan { fan: FANS[i].0 });
                }),
                options: FANS
                    .iter()
                    .map(|(_, l)| RadioItem { label: (*l).into(), ..Default::default() })
                    .collect(),
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Power limits & details…".into(),
                icon_name: "preferences-system-symbolic".into(),
                activate: Box::new(|this: &mut Self| this.open_window()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit panel".into(),
                activate: Box::new(|this: &mut Self| this.quit = true),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub fn run() {
    let tray = Tray { status: None, error: None, window: None, quit: false };
    let handle = match tray.spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("tpfand-panel: cannot create tray icon: {e}");
            eprintln!("(GNOME needs the AppIndicator extension enabled)");
            std::process::exit(1);
        }
    };
    let mut client: Option<Client> = None;
    loop {
        let res = match client.as_mut() {
            Some(c) => c.status(),
            None => Client::connect().and_then(|mut c| {
                let s = c.status();
                client = Some(c);
                s
            }),
        };
        if res.is_err() {
            client = None;
        }
        let quit = handle
            .update(|t| {
                match res {
                    Ok(s) => {
                        t.status = Some(s);
                        t.error = None;
                    }
                    Err(e) => {
                        t.status = None;
                        t.error = Some(e.to_string());
                    }
                }
                if let Some(c) = &mut t.window {
                    if !matches!(c.try_wait(), Ok(None)) {
                        t.window = None;
                    }
                }
                t.quit
            })
            .unwrap_or(true);
        if quit {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    handle.shutdown().wait();
}
