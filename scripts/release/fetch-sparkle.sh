#!/usr/bin/env bash
# Fetch the pinned Sparkle release used by the macOS 12+ application.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo 'Usage: fetch-sparkle.sh OUTPUT_DIRECTORY' >&2
  exit 2
fi
output=$1
version=2.10.0
sha256=c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c
license_sha256=389a4e4e9a32f059775b13a06e25a591445ba229d2838d26dd3e7c0c45127cfe
mkdir -p "$output"
archive="$output/Sparkle-$version.tar.xz"
curl --fail --location --silent --show-error --retry 3 \
  "https://github.com/sparkle-project/Sparkle/releases/download/$version/Sparkle-$version.tar.xz" \
  --output "$archive"
printf '%s  %s\n' "$sha256" "$archive" | shasum -a 256 --check
tar -xf "$archive" -C "$output"
printf '%s  %s\n' "$license_sha256" "$output/LICENSE" | shasum -a 256 --check
install -m 0644 "$output/LICENSE" "$output/Sparkle-LICENSE"
test -d "$output/Sparkle.framework"
test -x "$output/bin/sign_update"
