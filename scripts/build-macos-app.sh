#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'The macOS app must be built on macOS.' >&2
  exit 1
fi

cd "$(dirname "$0")/.."
if [[ "${1:-}" == "--no-build" ]]; then
  if [[ $# -ne 1 ]]; then
    echo 'Usage: build-macos-app.sh [--no-build]' >&2
    exit 2
  fi
else
  if [[ $# -ne 0 ]]; then
    echo 'Usage: build-macos-app.sh [--no-build]' >&2
    exit 2
  fi
  cargo build --release --locked --no-default-features --features full
fi

version=$(cargo metadata --offline --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "devknx") | .version')
if [[ -z "$version" ]]; then
  echo 'Could not read the devknx version.' >&2
  exit 1
fi

bundle=${DEVKNX_APP_OUTPUT:-dist/devknx.app}
if [[ -e "$bundle" ]]; then
  echo "The app output already exists: $bundle" >&2
  exit 1
fi
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources/licenses"
if [[ -n "${DEVKNX_SPARKLE_FRAMEWORK:-}" ]]; then
  if [[ ! -d "$DEVKNX_SPARKLE_FRAMEWORK" ]]; then
    echo "Sparkle.framework is missing: $DEVKNX_SPARKLE_FRAMEWORK" >&2
    exit 1
  fi
  mkdir -p "$bundle/Contents/Frameworks"
  ditto "$DEVKNX_SPARKLE_FRAMEWORK" "$bundle/Contents/Frameworks/Sparkle.framework"
  license="$(dirname "$DEVKNX_SPARKLE_FRAMEWORK")/Sparkle-LICENSE"
  if [[ ! -f "$license" ]]; then
    echo "Sparkle licence is missing: $license" >&2
    exit 1
  fi
  command cp "$license" "$bundle/Contents/Resources/Sparkle-LICENSE"
fi
command cp target/release/devknx "$bundle/Contents/MacOS/devknx"
command cp resources/devknx.icns "$bundle/Contents/Resources/devknx.icns"
command cp resources/Info.plist "$bundle/Contents/Info.plist"
command cp LICENSE THIRD-PARTY-NOTICES.md "$bundle/Contents/Resources/"
command cp licenses/* "$bundle/Contents/Resources/licenses/"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" \
  "$bundle/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" \
  "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"
"$bundle/Contents/MacOS/devknx" --version
