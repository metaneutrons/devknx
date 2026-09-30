#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 || ! -f $1 || ! $2 =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo 'Usage: qualify-deb.sh <package.deb> <expected-version>' >&2
  exit 2
fi
package=$1
version=$2
test "$(dpkg-deb --field "$package" Package)" = devknx
test "$(dpkg-deb --field "$package" Version)" = "${version}-1"
test "$(dpkg-deb --field "$package" Architecture)" = "$(dpkg --print-architecture)"

apt-get update
# Minimal Ubuntu images exclude /usr/share/doc by default. Keep this package's
# documentation during unpacking so the licence checks test the actual package,
# not the container image's deliberate documentation stripping.
apt-get install -y \
  -o 'Dpkg::Options::=--path-include=/usr/share/doc/devknx/*' "$package"
test "$(dpkg-query -W -f='${Version}' devknx)" = "${version}-1"
devknx --version | grep -Fx "devknx $version"
for notice in LICENSE THIRD-PARTY-NOTICES.md licenses/Hack.txt \
  licenses/OFL-1.1.txt licenses/UFL-1.0.txt licenses/emoji-icon-font-MIT.txt; do
  test -s "/usr/share/doc/devknx/$notice"
done
