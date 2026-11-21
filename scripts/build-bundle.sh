#!/bin/sh
# Builds target/Spool.driver (AudioServerPlugIn bundle) plus the spoold/spool binaries.
# Signs ad-hoc unless SIGN_IDENTITY is set (e.g. "Developer ID Application: ...").
set -eu
cd "$(dirname "$0")/.."

cargo build --release --workspace

BUNDLE=target/Spool.driver
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp bundle/Info.plist "$BUNDLE/Contents/Info.plist"
cp target/release/libspool_driver.dylib "$BUNDLE/Contents/MacOS/Spool"

IDENTITY="${SIGN_IDENTITY:--}"
if [ "$IDENTITY" = "-" ]; then
    codesign --force --sign - "$BUNDLE"
else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$BUNDLE"
fi
codesign --verify --verbose "$BUNDLE"
plutil -lint "$BUNDLE/Contents/Info.plist"
nm -gU "$BUNDLE/Contents/MacOS/Spool" | grep -q _SpoolDriverFactory \
    || { echo "factory symbol not exported" >&2; exit 1; }
echo "built $BUNDLE"
