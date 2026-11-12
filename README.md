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
| Dynamic Boost | 45 / 90 W base, up to 110 / 115 W | between Balanced and Performance |
| Performance | 115 / 115 W | always on, level 7 by 80°C — heavy builds, ML |

All values live in `/etc/tpfand.toml`.

Fan control details:

* Control temperature = max(average of all CPU core temps, dGPU temp). The
  hottest core is deliberately *not* used: Core Ultra parks one core at
  ~100°C during single-threaded boost at ~20 W (e.g. one rustc), which says
  nothing about heatsink load and would make the fan scream while compiling.
* Rising temps are smoothed over ~6 s so short bursts don't spike the fan;
  falling temps over ~10 s.
* Fan only steps down when 5°C below the threshold and after holding a level
  for 12 s (no hunting).
* **Critical**: smoothed control temp ≥94°C for 3 s (or any single sensor
  ≥106°C; TjMax is 110°C) →
  `full-speed` (disengaged, faster than level 7) in every profile and fan
  mode, until the smoothed temp drops below 85°C.
* The EC watchdog is armed: if the daemon dies, the fan returns to firmware
  auto within 20 s. On clean stop it hands the fan back immediately.
* Firmware rewrites power limits when the platform profile changes; tpfand
  re-applies its own every 10 s.

## Dynamic Boost

tpfand's own power mode (pin it from the tray or panel; it isn't a system
power mode, so Auto never selects it). It starts at the base limits and:

* **boosts** PL1 (in proportional steps) while the CPU is actually
  power-limited — package draw ≥85% of PL1 — and the control temp is under
  the 85°C target;
* **backs off** PL1 and trims the PL2 turbo budget at ≥89°C, and drops to
  base on critical;
* **decays** back to base after 15 s of light load, so the next burst starts
  from a sane place and idle stays quiet;
* **yields to the dGPU**: while the GPU is busy (≥20 W and ≥30% util, read
  via NVML on a separate thread), the CPU gets at most
  `total_budget_w − GPU draw` (130 W total by default), and if the GPU reports
  thermal slowdown the CPU steps down further (to a 25 W floor) so the shared
  heatsink has headroom for the GPU.

The RTX's own power cap can't be raised from userspace on laptop GPUs
(`nvidia-smi -pl` is refused, and NVIDIA's Dynamic Boost needs
`nvidia-powerd`), so "boost" here means re-balancing the shared thermal
budget, not exceeding the GPU's firmware cap. The panel shows the
controller's state, CPU/GPU draw, and lets you edit both the base limits and
the boost ceiling.

Measured on a P1 Gen 7 (all-core load, system Power Mode = Performance):
firmware Performance (115 W) hits 100°C and trips critical within ~15 s;
Dynamic Boost settles around 45–50 W sustained at ~85–88°C with the fan at
level 6–7 and never trips critical.

## Panel

`tpfand-panel` puts a tray icon in the top bar (GNOME needs the AppIndicator
extension, which Ubuntu ships):

* **Auto** — follow the system power mode (default; falls back to it whenever
  you unpin), or pin Power Saver / Balanced / Dynamic Boost / Performance.
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
