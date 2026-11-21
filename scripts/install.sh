#!/bin/sh
# Installs the driver (needs sudo), the binaries, and the per-user recorder LaunchAgent.
# Restarting coreaudiod briefly interrupts ALL audio on this Mac.
set -eu

restart_coreaudio() {
    # kickstart is refused under SIP on recent macOS; a plain SIGTERM makes launchd
    # restart coreaudiod just the same.
    sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null \
        || sudo killall coreaudiod
}
cd "$(dirname "$0")/.."

scripts/build-bundle.sh

HAL=/Library/Audio/Plug-Ins/HAL
LABEL=io.github.phil3k3.spoold
AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"

if cmp -s target/Spool.driver/Contents/MacOS/Spool "$HAL/Spool.driver/Contents/MacOS/Spool" \
    && cmp -s target/Spool.driver/Contents/Info.plist "$HAL/Spool.driver/Contents/Info.plist"; then
    echo "Driver unchanged, leaving coreaudiod alone."
else
    echo "Installing driver to $HAL (sudo)..."
    sudo rm -rf "$HAL/Spool.driver"
    sudo cp -R target/Spool.driver "$HAL/"
    sudo chown -R root:wheel "$HAL/Spool.driver"
    echo "Restarting coreaudiod (audio will drop out for a moment)..."
    restart_coreaudio
fi

sudo mkdir -p /usr/local/bin
sudo install -m 0755 target/release/spoold target/release/spool /usr/local/bin/

echo "Installing recorder agent $AGENT..."
mkdir -p "$HOME/Library/LaunchAgents"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
LOG_DIR="$HOME/Library/Logs/Spool"
mkdir -p "$LOG_DIR"
chmod 700 "$LOG_DIR"
cp "launchd/$LABEL.plist" "$AGENT"
plutil -replace StandardErrorPath -string "$LOG_DIR/spoold.log" "$AGENT"
launchctl bootstrap "gui/$(id -u)" "$AGENT"

echo
echo "Done. Select 'Spool' as the output device, then:"
echo "  spool start                      # listen (asks which device the first time)"
echo "  spool record                     # record to ~/Music/Spool (or: spool start --record)"
echo "  spool stop                       # stop both"
