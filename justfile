# tpfand tasks — `just` lists them.

default:
    @just --list

# Build and install daemon (root, via sudo/pkexec) + panel, restart both
install:
    ./install.sh

# Remove daemon and panel (config and state are kept)
uninstall:
    ./install.sh uninstall

# Rebuild and reinstall only the tray/panel (no root), then restart the tray
panel:
    cargo build --release -p tpfand-panel
    install -Dm755 target/release/tpfand-panel ~/.local/bin/tpfand-panel
    -pkill -x tpfand-panel
    systemd-run --user --collect --quiet ~/.local/bin/tpfand-panel

# Run tests and clippy
check:
    cargo test --workspace
    cargo clippy --workspace

# Daemon status
status:
    tpfand status
