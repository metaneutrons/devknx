#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'The macOS app must be built on macOS.' >&2
  exit 1
fi

cd "$(dirname "$0")/.."
cargo build --release --locked --no-default-features --features full

version=$(cargo metadata --offline --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "devknx") | .version')
if [[ -z "$version" ]]; then
  echo 'Could not read the devknx version.' >&2
  exit 1
fi

bundle=dist/devknx.app
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
command cp target/release/devknx "$bundle/Contents/MacOS/devknx"
command cp resources/devknx.icns "$bundle/Contents/Resources/devknx.icns"
command cp resources/Info.plist "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" \
  "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${version//./}" \
  "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"
"$bundle/Contents/MacOS/devknx" --version
