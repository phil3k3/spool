#!/bin/sh
# Installs the driver (needs sudo), the binaries, and the per-user recorder LaunchAgent.
# Restarting coreaudiod briefly interrupts ALL audio on this Mac.
#
#   --xpc   also install the XPC broker LaunchDaemon and build the driver with the XPC
#           probe (beta spike, docs/XPC.md). Installing without --xpc removes the broker.
#
# Identifier names come from the build (target/spool-id, see scripts/spool-id.sh). The
# installed prefix is recorded in /usr/local/share/spool/id, so that a changed prefix (a
# new remote, or SPOOL_ID_PREFIX) first removes the agent and broker installed under the
# old names.
set -eu

XPC=0
for arg in "$@"; do
    case "$arg" in
        --xpc) XPC=1 ;;
        *) echo "usage: $0 [--xpc]" >&2; exit 2 ;;
    esac
done

restart_coreaudio() {
    # kickstart is refused under SIP on recent macOS; a plain SIGTERM makes launchd
    # restart coreaudiod just the same.
    sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null \
        || sudo killall coreaudiod
}
cd "$(dirname "$0")/.."

SPOOL_XPC=$XPC scripts/build-bundle.sh
ID=$(cat target/spool-id)

HAL=/Library/Audio/Plug-Ins/HAL
LABEL=$ID.spoold
AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"
BROKER=$ID.spool-broker
BROKER_PLIST="/Library/LaunchDaemons/$BROKER.plist"
ID_FILE=/usr/local/share/spool/id

OLD_ID=$(cat "$ID_FILE" 2>/dev/null || true)
if [ -n "$OLD_ID" ] && [ "$OLD_ID" != "$ID" ]; then
    echo "Identifier prefix changed from $OLD_ID to $ID; removing the old agent and broker..."
    launchctl bootout "gui/$(id -u)/$OLD_ID.spoold" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$OLD_ID.spoold.plist"
    sudo launchctl bootout "system/$OLD_ID.spool-broker" 2>/dev/null || true
    sudo rm -f "/Library/LaunchDaemons/$OLD_ID.spool-broker.plist"
fi

# The broker goes first: the driver looks it up once, when coreaudiod loads it below.
if [ "$XPC" = 1 ]; then
    echo "Installing XPC broker $BROKER_PLIST (sudo, beta)..."
    sudo mkdir -p /usr/local/libexec
    sudo install -m 0755 target/release/spool-broker /usr/local/libexec/
    sudo launchctl bootout "system/$BROKER" 2>/dev/null || true
    sudo install -m 0644 -o root -g wheel target/launchd/spool-broker.plist "$BROKER_PLIST"
    sudo launchctl bootstrap system "$BROKER_PLIST"
elif [ -e "$BROKER_PLIST" ]; then
    echo "Removing XPC broker (installed without --xpc)..."
    sudo launchctl bootout "system/$BROKER" 2>/dev/null || true
    sudo rm -f "$BROKER_PLIST" /usr/local/libexec/spool-broker
fi

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
sudo mkdir -p "$(dirname "$ID_FILE")"
printf '%s\n' "$ID" | sudo tee "$ID_FILE" >/dev/null

echo "Installing recorder agent $AGENT..."
mkdir -p "$HOME/Library/LaunchAgents"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
LOG_DIR="$HOME/Library/Logs/Spool"
mkdir -p "$LOG_DIR"
chmod 700 "$LOG_DIR"
cp target/launchd/spoold.plist "$AGENT"
plutil -replace StandardErrorPath -string "$LOG_DIR/spoold.log" "$AGENT"
launchctl bootstrap "gui/$(id -u)" "$AGENT"

echo
echo "Done ($ID). Select 'Spool' as the output device, then:"
echo "  spool start                      # listen (asks which device the first time)"
echo "  spool record                     # record to ~/Music/Spool (or: spool start --record)"
echo "  spool stop                       # stop both"
if [ "$XPC" = 1 ]; then
    echo
    echo "XPC spike: check the result with"
    echo "  /usr/local/libexec/spool-broker --probe"
    echo "  log show --last 5m --predicate 'eventMessage CONTAINS \"Spool\"'"
fi
