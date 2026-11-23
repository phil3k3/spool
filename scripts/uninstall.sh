#!/bin/sh
# Removes the driver, binaries, LaunchAgent and XPC broker. Recordings in ~/Music/Spool are kept.
set -eu
cd "$(dirname "$0")/.."

restart_coreaudio() {
    # kickstart is refused under SIP on recent macOS; a plain SIGTERM makes launchd
    # restart coreaudiod just the same.
    sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null \
        || sudo killall coreaudiod
}
# The prefix that was installed, else the one this checkout would build with.
ID=$(cat /usr/local/share/spool/id 2>/dev/null || scripts/spool-id.sh)
LABEL=$ID.spoold
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
sudo rm -rf /Library/Audio/Plug-Ins/HAL/Spool.driver
sudo rm -f /usr/local/bin/spoold /usr/local/bin/spool
BROKER=$ID.spool-broker
sudo launchctl bootout "system/$BROKER" 2>/dev/null || true
sudo rm -f "/Library/LaunchDaemons/$BROKER.plist" /usr/local/libexec/spool-broker
sudo rm -rf /usr/local/share/spool
echo "Restarting coreaudiod (audio will drop out for a moment)..."
restart_coreaudio
echo "Uninstalled."
