#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
  echo 'The native menu smoke test requires macOS.' >&2
  exit 1
fi

cd "$(dirname "$0")/.."
output=$(mktemp "${TMPDIR:-/tmp}/devknx-menu-smoke.XXXXXX")
trap 'command rm -f "$output"' EXIT
clang -Wall -Wextra -Werror -framework AppKit resources/macos.m \
  tests/macos-menu-smoke.m -o "$output"
"$output"
