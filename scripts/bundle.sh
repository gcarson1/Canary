#!/bin/sh
# Builds target/release/Canary.app and signs it.
#   scripts/bundle.sh                       ad-hoc signature (runs on this Mac)
#   SIGN_IDENTITY="Developer ID Application: …" scripts/bundle.sh
#                                           hardened runtime, ready to notarize
set -eu
cd "$(dirname "$0")/.."

cargo build --release -p canary
version=$(cargo metadata --no-deps --format-version 1 \
  | sed -n 's/.*"name":"canary","version":"\([^"]*\)".*/\1/p')

app=target/release/Canary.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/canary "$app/Contents/MacOS/canary"
sed "s/__VERSION__/${version:-0.1.0}/g" bundle/Info.plist > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

if [ -n "${SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp \
    --entitlements bundle/Canary.entitlements --sign "$SIGN_IDENTITY" "$app"
else
  codesign --force --entitlements bundle/Canary.entitlements --sign - "$app"
fi
codesign --verify --strict --verbose=2 "$app"
plutil -lint "$app/Contents/Info.plist"
echo "built $app"
