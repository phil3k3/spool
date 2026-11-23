#!/bin/sh
# Builds target/Spool.driver (AudioServerPlugIn bundle) plus the spoold/spool binaries.
# Signs ad-hoc unless SIGN_IDENTITY is set (e.g. "Developer ID Application: ...").
# SPOOL_XPC=1 builds the driver with the XPC probe (docs/XPC.md).
#
# Identifiers (bundle ID, launchd labels, Mach service) use the prefix from
# scripts/spool-id.sh, i.e. the `origin` remote or $SPOOL_ID_PREFIX. The plists are
# generated from templates into target/, and the prefix is recorded in target/spool-id
# for scripts/install.sh.
set -eu
cd "$(dirname "$0")/.."

ID=$(scripts/spool-id.sh)
# Exported so crates/mach/build.rs bakes in exactly this prefix (and rebuilds if it changed).
export SPOOL_ID_PREFIX="$ID"
echo "identifier prefix: $ID"

if [ "${SPOOL_XPC:-0}" = 1 ]; then
    cargo build --release --workspace --features spool-driver/xpc-probe
else
    cargo build --release --workspace
fi

BUNDLE=target/Spool.driver
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
gen() {
    sed "s/@SPOOL_ID@/$ID/g" "$1" > "$2"
    if grep -q @SPOOL_ID@ "$2"; then echo "unreplaced @SPOOL_ID@ in $2" >&2; exit 1; fi
    plutil -lint -s "$2"
}
gen bundle/Info.plist "$BUNDLE/Contents/Info.plist"
rm -rf target/launchd
mkdir -p target/launchd
gen launchd/spoold.plist target/launchd/spoold.plist
gen launchd/spool-broker.plist target/launchd/spool-broker.plist
# The broker binary must carry the same service name as the plists.
grep -qF "$ID.spool-broker" target/release/spool-broker \
    || { echo "spool-broker was not built with prefix $ID" >&2; exit 1; }
printf '%s\n' "$ID" > target/spool-id
cp target/release/libspool_driver.dylib "$BUNDLE/Contents/MacOS/Spool"

IDENTITY="${SIGN_IDENTITY:--}"
if [ "$IDENTITY" = "-" ]; then
    codesign --force --sign - "$BUNDLE"
else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$BUNDLE"
fi
codesign --verify --verbose "$BUNDLE"
nm -gU "$BUNDLE/Contents/MacOS/Spool" | grep -q _SpoolDriverFactory \
    || { echo "factory symbol not exported" >&2; exit 1; }
echo "built $BUNDLE ($ID.spool.driver)"
