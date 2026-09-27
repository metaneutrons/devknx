#!/usr/bin/env bash
set -euo pipefail

# Package a native Linux build without rebuilding it. The archive repository
# identifies a payload by this exact Debian file name and its control fields.
if [[ $# -ne 2 ]]; then
  echo 'Usage: build-deb.sh <release-tag> <output-directory>' >&2
  exit 2
fi

tag=$1
output_dir=$2
if [[ ! $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid release tag: $tag" >&2
  exit 1
fi
version=${tag#v}
version=${version%%-*}
manifest_version=$(cargo metadata --offline --no-deps --locked --format-version 1 |
  jq -r '.packages[] | select(.name == "devknx") | .version')
if [[ $version != "$manifest_version" ]]; then
  echo "Tag version $version does not match Cargo.toml $manifest_version" >&2
  exit 1
fi
if [[ ! -x target/release/devknx ]]; then
  echo 'Native release binary is missing.' >&2
  exit 1
fi
if [[ $(target/release/devknx --version) != "devknx $version" ]]; then
  echo 'Native release binary version does not match the tag.' >&2
  exit 1
fi

arch=$(dpkg --print-architecture)
if [[ $arch != amd64 && $arch != arm64 ]]; then
  echo "Unsupported Debian architecture: $arch" >&2
  exit 1
fi
output=$output_dir/devknx_${version}-1_${arch}.deb
if [[ -e $output ]]; then
  echo "Refusing to overwrite existing package: $output" >&2
  exit 1
fi
mkdir -p "$output_dir"
cargo deb --no-build --deb-version "${version}-1" --output "$output"

if [[ $(dpkg-deb --field "$output" Package) != devknx ||
      $(dpkg-deb --field "$output" Version) != "${version}-1" ||
      $(dpkg-deb --field "$output" Architecture) != "$arch" ]]; then
  echo 'Debian control identity does not match the file name.' >&2
  exit 1
fi
members_text=$(ar t "$output")
mapfile -t members <<< "$members_text"
if [[ ${#members[@]} -ne 3 ||
      ${members[0]} != debian-binary ||
      ! ${members[1]} =~ ^control\.tar\.(gz|xz|bz2|zst)$ ||
      ! ${members[2]} =~ ^data\.tar\.(gz|xz|bz2|zst)$ ]]; then
  echo 'Unexpected Debian archive member layout.' >&2
  exit 1
fi
contents=$(dpkg-deb --contents "$output")
grep -F './usr/bin/devknx' <<< "$contents" >/dev/null
grep -F './usr/share/doc/devknx/THIRD-PARTY-NOTICES.md' <<< "$contents" >/dev/null
printf '%s\n' "$output"
