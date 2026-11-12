#!/usr/bin/env bash
# Build and install tpfand (daemon, system-wide) and tpfand-panel (current user).
#   ./install.sh            install / upgrade
#   ./install.sh uninstall  remove everything (fan returns to firmware control)
set -euo pipefail
cd "$(dirname "$0")"

SUDO=sudo
[ "$(id -u)" = 0 ] && SUDO=
[ -n "$SUDO" ] && ! sudo -n true 2>/dev/null && command -v pkexec >/dev/null && SUDO=pkexec

if [ "${1:-}" = uninstall ]; then
    $SUDO systemctl disable --now tpfand.service || true
    $SUDO rm -f /usr/local/bin/tpfand /etc/systemd/system/tpfand.service
    $SUDO systemctl daemon-reload
    rm -f ~/.local/bin/tpfand-panel ~/.config/autostart/tpfand-tray.desktop \
          ~/.local/share/applications/tpfand-panel.desktop
    pkill -x tpfand-panel || true
    echo "Removed. /etc/tpfand.toml, /etc/modprobe.d/thinkpad-fan.conf and /var/lib/tpfand were kept."
    exit 0
fi

cargo build --release

# --- daemon (root)
$SUDO sh -c '
set -e
cd "$1"
install -Dm755 target/release/tpfand /usr/local/bin/tpfand
install -Dm644 dist/tpfand.service /etc/systemd/system/tpfand.service
[ -f /etc/tpfand.toml ] || install -Dm644 crates/tpfand/tpfand.toml /etc/tpfand.toml
if [ ! -f /etc/modprobe.d/thinkpad-fan.conf ]; then
    install -Dm644 dist/thinkpad-fan.conf /etc/modprobe.d/thinkpad-fan.conf
fi
if [ "$(cat /sys/module/thinkpad_acpi/parameters/fan_control 2>/dev/null)" != Y ]; then
    modprobe -r thinkpad_acpi && modprobe thinkpad_acpi
fi
/usr/local/bin/tpfand check
systemctl daemon-reload
systemctl enable tpfand.service
systemctl restart tpfand.service
' sh "$PWD"

# --- panel (user)
install -Dm755 target/release/tpfand-panel ~/.local/bin/tpfand-panel
install -Dm644 dist/tpfand-panel.desktop ~/.local/share/applications/tpfand-panel.desktop
install -Dm644 dist/tpfand-tray.desktop ~/.config/autostart/tpfand-tray.desktop
sed -i "s|^Exec=tpfand-panel|Exec=$HOME/.local/bin/tpfand-panel|" \
    ~/.local/share/applications/tpfand-panel.desktop ~/.config/autostart/tpfand-tray.desktop

if command -v gnome-extensions >/dev/null &&
   gnome-extensions list | grep -q ubuntu-appindicators@ubuntu.com &&
   ! gnome-extensions list --enabled | grep -q ubuntu-appindicators@ubuntu.com; then
    echo "Enabling the AppIndicator extension (needed for the tray icon on GNOME)"
    gnome-extensions enable ubuntu-appindicators@ubuntu.com || true
fi

pkill -x tpfand-panel || true
# Launch through the user manager so it gets WAYLAND_DISPLAY etc. even when
# this script runs from a non-graphical shell.
systemd-run --user --collect --quiet ~/.local/bin/tpfand-panel ||
    setsid -f ~/.local/bin/tpfand-panel >/dev/null 2>&1 < /dev/null
echo "Installed. Status: tpfand status   Logs: journalctl -u tpfand -f"
