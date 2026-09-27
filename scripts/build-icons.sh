#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
mkdir -p resources/devknx.iconset resources/png target

for spec in \
  '16:icon_16x16.png' \
  '32:icon_16x16@2x.png' \
  '32:icon_32x32.png' \
  '64:icon_32x32@2x.png' \
  '128:icon_128x128.png' \
  '256:icon_128x128@2x.png' \
  '256:icon_256x256.png' \
  '512:icon_256x256@2x.png' \
  '512:icon_512x512.png' \
  '1024:icon_512x512@2x.png'; do
  size=${spec%%:*}
  name=${spec#*:}
  sips -z "$size" "$size" resources/icon-master.png \
    --out "resources/devknx.iconset/$name" >/dev/null
done

for size in 16 32 48 64 128 256 512; do
  sips -z "$size" "$size" resources/icon-master.png \
    --out "resources/png/devknx-${size}.png" >/dev/null
done

iconutil --convert icns --output resources/devknx.icns resources/devknx.iconset
rustc --edition=2024 scripts/pack-ico.rs -o target/icon-pack
target/icon-pack resources/devknx.ico \
  resources/png/devknx-16.png \
  resources/png/devknx-32.png \
  resources/png/devknx-48.png \
  resources/png/devknx-256.png
