#!/bin/bash
# Build an mw75 binary, wrap it in a proper macOS .app bundle, and sign it
# with the Bluetooth entitlement.
#
# WHY THIS EXISTS
# ---------------
# On macOS Sequoia/Tahoe (15/26+), opening a Classic Bluetooth RFCOMM channel
# (IOBluetooth) requires the calling process to hold the "Bluetooth" TCC
# privacy grant. A bare CLI — even ad-hoc signed with an embedded Info.plist —
# is never registered with TCC, so openRFCOMMChannelSync returns
# kIOReturnNotPermitted (0xe00002bc). Only a real .app *bundle* (with
# CFBundleIdentifier + NSBluetoothAlwaysUsageDescription) triggers the system
# permission prompt and persists the grant. This wraps the CLI in such a bundle.
#
# Usage:
#   ./macos/make-app.sh                  # bundle the `mw75` binary
#   ./macos/make-app.sh rfcomm-debug     # bundle a different binary
#   ./macos/make-app.sh --release mw75   # release build
#
# After building, run it once so macOS shows the Bluetooth prompt:
#   open ./build/MW75.app                # GUI launch (gets the TCC prompt)
#   ./build/MW75.app/Contents/MacOS/<bin>  # or run the binary directly
# Click "Allow" on the Bluetooth prompt; the grant then persists by bundle id.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
ENTITLEMENTS="$SCRIPT_DIR/entitlements.plist"

BIN_NAME="mw75"
BUILD_FLAGS="--features rfcomm"
PROFILE="debug"
for arg in "$@"; do
    case "$arg" in
        --release) BUILD_FLAGS="$BUILD_FLAGS --release"; PROFILE="release" ;;
        -*)        BUILD_FLAGS="$BUILD_FLAGS $arg" ;;
        *)         BIN_NAME="$arg" ;;
    esac
done

cd "$PROJECT_DIR"
echo "═══ Building $BIN_NAME ($PROFILE) ═══"
cargo build --bin "$BIN_NAME" $BUILD_FLAGS

BINARY="target/$PROFILE/$BIN_NAME"
[ -f "$BINARY" ] || { echo "ERROR: $BINARY not found"; exit 1; }

APP="build/MW75.app"
echo "═══ Assembling $APP ═══"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"

cp "$BINARY" "$APP/Contents/MacOS/$BIN_NAME"

# A complete Info.plist (CFBundleExecutable + package type make it a real bundle).
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>          <string>$BIN_NAME</string>
    <key>CFBundleIdentifier</key>          <string>com.mw75.eeg</string>
    <key>CFBundleName</key>                <string>MW75 EEG</string>
    <key>CFBundlePackageType</key>         <string>APPL</string>
    <key>CFBundleVersion</key>             <string>0.0.6</string>
    <key>CFBundleShortVersionString</key>  <string>0.0.6</string>
    <key>LSMinimumSystemVersion</key>      <string>12.0</string>
    <key>NSBluetoothAlwaysUsageDescription</key>
    <string>MW75 EEG needs Bluetooth to connect to MW75 Neuro headphones for EEG data streaming via RFCOMM.</string>
    <key>NSBluetoothPeripheralUsageDescription</key>
    <string>MW75 EEG needs Bluetooth to connect to MW75 Neuro headphones.</string>
</dict>
</plist>
PLIST

echo "═══ Signing bundle with Bluetooth entitlement ═══"
codesign --force --deep --sign - \
    --entitlements "$ENTITLEMENTS" \
    --options runtime \
    "$APP"

echo "  ✅ Signed"
codesign -d --entitlements - "$APP" 2>&1 | grep -i bluetooth || true

cat <<DONE

═══ Done ═══
  Bundle:     $PROJECT_DIR/$APP
  Run (GUI, triggers Bluetooth prompt):  open "$APP"
  Run (direct):                          "$APP/Contents/MacOS/$BIN_NAME"

  First run will prompt for Bluetooth access — click "Allow".
  If no prompt appears, add the bundle under
  System Settings → Privacy & Security → Bluetooth.
DONE
