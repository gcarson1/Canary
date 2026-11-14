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

# Bundled instruments (scripts/fetch_plugins.sh), copied with their own
# developers' signatures intact. Without them Canary still builds and runs.
if ls third_party/plugins/*.clap >/dev/null 2>&1; then
  mkdir -p "$app/Contents/PlugIns"
  for plugin in third_party/plugins/*.clap; do
    copy="$app/Contents/PlugIns/$(basename "$plugin")"
    ditto "$plugin" "$copy"
    # Some release builds (Dexed) ship unsigned; sign those like the app.
    codesign --verify --strict "$copy" 2>/dev/null ||
      codesign --force --sign "${SIGN_IDENTITY:--}" "$copy"
  done
else
  echo "no bundled plugins: run scripts/fetch_plugins.sh to include them" >&2
fi

if [ -n "${SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp \
    --entitlements bundle/Canary.entitlements --sign "$SIGN_IDENTITY" "$app"
else
  codesign --force --entitlements bundle/Canary.entitlements --sign - "$app"
fi
codesign --verify --strict --verbose=2 "$app"
plutil -lint "$app/Contents/Info.plist"
echo "built $app"
