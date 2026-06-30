#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf 'usage: scripts/package-release.sh <rust-target> <asset-platform>\n' >&2
  exit 2
fi

target=$1
asset_platform=$2

case "$asset_platform" in
  '' | *[!A-Za-z0-9._-]*)
    printf 'asset platform contains characters unsafe for asset names: %s\n' "$asset_platform" >&2
    exit 2
    ;;
  *unknown*)
    # Rust target triples include a vendor field (`unknown` for the targets we
    # use), but that is compiler plumbing rather than user-facing release
    # identity. Require the caller to pass the installer-facing platform label
    # from the release matrix so published assets stay readable and stable.
    printf 'asset platform must be a public release label, not a Rust target triple: %s\n' "$asset_platform" >&2
    exit 2
    ;;
esac

tag=$(scripts/release-tag.sh)
commit=${HIVE_MEMORY_BUILD_COMMIT:-${GITHUB_SHA:-}}
if [[ -z "$commit" ]]; then
  commit=$(git rev-parse HEAD)
fi
if [[ ! "$commit" =~ ^[0-9a-fA-F]{8,}$ ]]; then
  printf 'build commit must be a concrete git hash, got %s\n' "$commit" >&2
  exit 1
fi

asset="hm-${tag}-${asset_platform}.tar.gz"
dist_dir=dist
staging=$(mktemp -d)

cleanup() {
  rm -rf "$staging"
}
trap cleanup EXIT

# Release assets are the contract dotfiles/shdeps will consume. Build the exact
# target binary first, then package only the executable and minimal project
# metadata so bootstrap does not require a Rust toolchain or source checkout.
HIVE_MEMORY_BUILD_COMMIT="$commit" HIVE_MEMORY_BUILD_VERSION="$tag" \
  cargo build --release --locked --target "$target"

install -m 0755 "target/${target}/release/hm" "$staging/hm"
install -m 0644 README.md "$staging/README.md"
install -m 0644 LICENSE "$staging/LICENSE"
mkdir -p "$staging/man/man1"
install -m 0644 man/man1/hm.1 "$staging/man/man1/hm.1"

mkdir -p "$dist_dir"
# Local package+smoke loops should test the archive that was actually produced,
# even when the caller is using explicit build env or a source snapshot without
# Git metadata. Real tag releases remain governed by the GitHub tag and do not
# depend on this ignored breadcrumb.
printf '%s\n' "$tag" >"${dist_dir}/.hive-memory-release-version"
tar -C "$staging" -czf "${dist_dir}/${asset}" .

# GNU coreutils and macOS expose different checksum commands. Prefer
# `sha256sum` when present, but keep the archive script native on macOS runners
# so release creation does not depend on Homebrew bootstrap state.
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist_dir" && sha256sum "$asset") >"${dist_dir}/${asset}.sha256"
else
  (cd "$dist_dir" && shasum -a 256 "$asset") >"${dist_dir}/${asset}.sha256"
fi

printf '%s\n' "${dist_dir}/${asset}"
