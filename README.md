# tpfand

Profile-aware fan curves and CPU power limits (TDP) for ThinkPads on Linux,
with a tray icon + egui control panel.

Built for a ThinkPad P1 Gen 7 (Core Ultra 7 155H), but should work on any
ThinkPad with `thinkpad_acpi` fan control and Intel RAPL.

## What it does

`tpfand` (root daemon) follows the system power mode — the
`/sys/firmware/acpi/platform_profile` that GNOME's Power Mode menu /
power-profiles-daemon sets — or a profile you pin from the panel, and applies:

| Profile     | PL1 / PL2 | Fan curve |
|-------------|-----------|-----------|
| Power Saver | 15 / 30 W | off until 58°C, gentle ramp — browsing, notes, light coding |
| Balanced    | 45 / 80 W | off until 52°C, moderate — class, coding, normal builds |
| Performance | 115 / 115 W | always on, level 7 by 80°C — heavy builds, ML |

All values live in `/etc/tpfand.toml`.

Fan control details:

* Temperature = max of CPU package (coretemp) and the EC's CPU/GPU sensors.
* Rising temps are smoothed over ~6 s so short compile bursts and single-core
  boost spikes (Core Ultra jumps 50→100°C in a second at ~20 W) don't make
  the fan spike; falling temps over ~10 s.
* Fan only steps down when 5°C below the threshold and after holding a level
  for 12 s (no hunting).
* **Critical**: smoothed temp ≥94°C for 3 s (or any raw reading ≥106°C) →
  `full-speed` (disengaged, faster than level 7) in every profile and fan
  mode, until the smoothed temp drops below 85°C.
* The EC watchdog is armed: if the daemon dies, the fan returns to firmware
  auto within 20 s. On clean stop it hands the fan back immediately.
* Firmware rewrites power limits when the platform profile changes; tpfand
  re-applies its own every 10 s.

## Panel

`tpfand-panel` puts a tray icon in the top bar (GNOME needs the AppIndicator
extension, which Ubuntu ships):

* **Auto** — follow the system power mode (default; falls back to it whenever
  you unpin), or pin Power Saver / Balanced / Performance.
* Fan: profile curve, maximum, or firmware auto.
* "Power limits & details…" opens an egui window with live temps, fan RPM,
  package power, and PL1/PL2 sliders per profile (apply / reset to default).

Choices persist in `/var/lib/tpfand/state.json`.

## IPC

Unix socket `/run/tpfand.sock`, newline-delimited JSON (see
`crates/tpfand-proto`). Anyone may read status; changes require root or
membership in `allow_group` (default `sudo`), checked with `SO_PEERCRED`.

```sh
echo '{"cmd":"status"}' | socat - UNIX-CONNECT:/run/tpfand.sock
echo '{"cmd":"set-mode","profile":"performance"}' | socat - UNIX-CONNECT:/run/tpfand.sock
echo '{"cmd":"set-mode","profile":null}' | socat - UNIX-CONNECT:/run/tpfand.sock   # auto
echo '{"cmd":"set-limits","profile":"balanced","limits":{"pl1_w":40,"pl2_w":70}}' | socat - UNIX-CONNECT:/run/tpfand.sock
```

## Install

```sh
./install.sh            # builds, installs daemon + panel, enables service
tpfand status
journalctl -u tpfand -f
./install.sh uninstall
```

Requires `thinkpad_acpi` with `fan_control=1`; the installer writes
`/etc/modprobe.d/thinkpad-fan.conf` and reloads the module if needed.

## Layout

* `crates/tpfand` — daemon (`run`, `status`, `check`)
* `crates/tpfand-proto` — shared IPC types + blocking client
* `crates/tpfand-panel` — tray (ksni) + window (eframe/egui)
