#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! -f $1 || ! -x $1 ]]; then
  echo 'Usage: qualify-linux-gui.sh <installed-executable>' >&2
  exit 2
fi
executable=$1
smoke_root=$(mktemp -d)
mkdir -m 700 "$smoke_root/runtime"
run_isolated() {
  env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
    HOME="$smoke_root" XDG_DATA_HOME="$smoke_root/data" \
    XDG_CONFIG_HOME="$smoke_root/config" XDG_RUNTIME_DIR="$smoke_root/runtime" \
    LIBGL_ALWAYS_SOFTWARE=1 "$@"
}
cleanup() {
  # GUI close intentionally leaves a daemon alive during normal use. Stop only
  # a daemon in this test's isolated data directory, never the user's daemon.
  run_isolated timeout --kill-after=5s 10s "$executable" daemon --stop >/dev/null 2>&1 || true
  command rm -r -- "$smoke_root"
}
trap cleanup EXIT
run_isolated timeout --kill-after=5s 30s xvfb-run -a "$executable" gui --smoke
