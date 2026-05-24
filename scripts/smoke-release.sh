#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/smoke-release.sh <asset-platform>" >&2
  exit 2
fi

asset_platform=$1
version=$(scripts/cargo-version.sh)
archive="dist/hm-v${version}-${asset_platform}.tar.gz"

mkdir -p smoke
tar -xzf "$archive" -C smoke

./smoke/hm --version
./smoke/hm stores init personal --root "$PWD/smoke-store"

cat >smoke-config.toml <<EOF
default_store = "personal"

[stores.personal]
root = "$PWD/smoke-store"
EOF

./smoke/hm --config "$PWD/smoke-config.toml" doctor --quick
