#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: scripts/smoke-release.sh <asset-platform>\n' >&2
  exit 2
fi

asset_platform=$1
case "$asset_platform" in
  *unknown*)
    # Smoke tests run against the same label that gets uploaded. Reject target
    # triples here too so a workflow edit cannot package a clean label but smoke
    # a different, Rust-internal archive name by mistake.
    printf 'asset platform must be a public release label, not a Rust target triple: %s\n' "$asset_platform" >&2
    exit 2
    ;;
esac

tag=$(scripts/release-tag.sh)
archive="dist/hm-${tag}-${asset_platform}.tar.gz"
if [[ ! -f "$archive" && -f dist/.hive-memory-release-version ]]; then
  # Local dry-runs are not anchored by a pushed release tag. Reuse the package
  # script's recorded version so smoke always verifies the archive produced by
  # the immediately preceding package step.
  tag=$(<dist/.hive-memory-release-version)
  case "$tag" in
    '' | *[!A-Za-z0-9._-]*)
      printf 'recorded release version is unsafe for asset names: %s\n' "$tag" >&2
      exit 2
      ;;
  esac
  archive="dist/hm-${tag}-${asset_platform}.tar.gz"
fi
smoke=$(mktemp -d)
smoke_store=$(mktemp -d)
smoke_config=$(mktemp)

cleanup() {
  rm -rf "$smoke" "$smoke_store" "$smoke_config"
}
trap cleanup EXIT

tar -xzf "$archive" -C "$smoke"

test -f "$smoke/man/man1/hm.1"
"$smoke/hm" --version
"$smoke/hm" stores init personal --root "$smoke_store"

cat >"$smoke_config" <<EOF
default_store = "personal"

[stores.personal]
root = "$smoke_store"
EOF

"$smoke/hm" --config "$smoke_config" doctor --quick
