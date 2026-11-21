#!/bin/sh
# Removes the driver, binaries and LaunchAgent. Recordings in ~/Music/Spool are kept.
set -eu

restart_coreaudio() {
    # kickstart is refused under SIP on recent macOS; a plain SIGTERM makes launchd
    # restart coreaudiod just the same.
    sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null \
        || sudo killall coreaudiod
}
LABEL=io.github.phil3k3.spoold
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
sudo rm -rf /Library/Audio/Plug-Ins/HAL/Spool.driver
sudo rm -f /usr/local/bin/spoold /usr/local/bin/spool
echo "Restarting coreaudiod (audio will drop out for a moment)..."
restart_coreaudio
echo "Uninstalled."
