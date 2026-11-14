#!/bin/sh
# Downloads the bundled instruments (pinned, checksummed official macOS builds)
# and unpacks their CLAP bundles into third_party/plugins, where `cargo run`
# finds them and scripts/bundle.sh copies them into Canary.app. Their factory
# data (presets, wavetables) goes to ~/Library/Application Support, where the
# plugins look for it, unless it is already there. Installs nothing system-wide.
#   scripts/fetch_plugins.sh
set -eu
cd "$(dirname "$0")/.."

out=third_party/plugins
cache=third_party/plugins/.cache
support="$HOME/Library/Application Support"
mkdir -p "$cache"

# fetch NAME URL SHA256: downloads once into the cache and checks the sum.
fetch() {
  file="$cache/$1"
  if [ ! -f "$file" ]; then
    curl -fL --retry 3 -o "$file.part" "$2"
    mv "$file.part" "$file"
  fi
  echo "$3  $file" | shasum -a 256 -c - >/dev/null || {
    echo "checksum mismatch: $file" >&2
    rm -f "$file"
    exit 1
  }
}

# A disk image mounted by expand_dmg_pkg; detached however the script ends.
mnt=""
cleanup() {
  if [ -n "$mnt" ]; then
    hdiutil detach "$mnt" >/dev/null 2>&1 || true
    rmdir "$mnt" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# expand_dmg_pkg DMG DIR: the installer package inside DMG, expanded to DIR
# without running it.
expand_dmg_pkg() {
  mnt=$(mktemp -d)
  rm -rf "$2"
  hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$1" >/dev/null
  pkgutil --expand-full "$mnt"/*.pkg "$2"
  cleanup
  mnt=""
}

# place SRC DEST: copies a bundle or folder, keeping code signatures. Copies
# beside DEST then renames, so an interrupted run never leaves half a copy.
place() {
  mkdir -p "$(dirname "$2")"
  rm -rf "$2.part"
  ditto "$1" "$2.part"
  rm -rf "$2"
  mv "$2.part" "$2"
}

# Surge XT 1.3.4 (GPL-3.0): synth and effects.
fetch surge-xt-1.3.4.dmg \
  https://github.com/surge-synthesizer/releases-xt/releases/download/1.3.4/surge-xt-macOS-1.3.4.dmg \
  b56e8d51a8dc2382ae668c9ff4e96bae38dbd452af4e584d5a5ef50ab2d09b07
x="$cache/surge"
expand_dmg_pkg "$cache/surge-xt-1.3.4.dmg" "$x"
place "$x/Surge_XT_CLAP.pkg/Payload/Surge XT.clap" "$out/Surge XT.clap"
place "$x/Surge_XT_FXCLAP.pkg/Payload/Surge XT Effects.clap" "$out/Surge XT Effects.clap"
[ -d "$support/Surge XT" ] || place "$x/Surge_XT_Resources.pkg/Payload" "$support/Surge XT"
rm -rf "$x"

# Dexed 1.1.0 (GPL-3.0): FM synth, presets built in.
fetch dexed-1.1.0.zip \
  https://github.com/asb2m10/dexed/releases/download/v1.1.0/Dexed-1.1.0-macOS.zip \
  d6576d064313327e216cec9d6f62caa650b44746bc7c8d4b010df7f55d9b8ae3
x="$cache/dexed"
rm -rf "$x"
ditto -x -k "$cache/dexed-1.1.0.zip" "$x"
place "$x/Dexed.clap" "$out/Dexed.clap"
rm -rf "$x"

# OB-Xf 1.0.3 (GPL-3.0): analog-style poly synth.
fetch ob-xf-1.0.3.dmg \
  https://github.com/surge-synthesizer/OB-Xf/releases/download/v1.0.3/ob-xf-macOS-v1.0.3.dmg \
  e6f52f453c476dfafbd3225b80c5c3ec341ec20be5748c6f70c01cef14e7e7a8
x="$cache/obxf"
expand_dmg_pkg "$cache/ob-xf-1.0.3.dmg" "$x"
place "$x/ob-xf_CLAP.pkg/Payload/OB-Xf.clap" "$out/OB-Xf.clap"
[ -d "$support/Surge Synth Team/OB-Xf" ] ||
  place "$x/ob-xf_AppSupport.pkg/Payload/Surge Synth Team/OB-Xf" "$support/Surge Synth Team/OB-Xf"
rm -rf "$x"

ls -d "$out"/*.clap
