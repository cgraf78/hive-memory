#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/package-release.sh <rust-target>" >&2
  exit 2
fi

target=$1
version=$(scripts/cargo-version.sh)

asset="hm-v${version}-${target}.tar.gz"
dist_dir=dist
staging=$(mktemp -d)

cleanup() {
  rm -rf "$staging"
}
trap cleanup EXIT

# Release assets are the contract dotfiles/shdeps will consume. Build the exact
# target binary first, then package only the executable and minimal project
# metadata so bootstrap does not require a Rust toolchain or source checkout.
cargo build --release --locked --target "$target"

install -m 0755 "target/${target}/release/hm" "$staging/hm"
install -m 0644 README.md "$staging/README.md"
install -m 0644 LICENSE "$staging/LICENSE"

mkdir -p "$dist_dir"
tar -C "$staging" -czf "${dist_dir}/${asset}" .

# GNU coreutils and macOS expose different checksum commands. Prefer
# `sha256sum` when present, but keep the archive script native on macOS runners
# so release creation does not depend on Homebrew bootstrap state.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${dist_dir}/${asset}" >"${dist_dir}/${asset}.sha256"
else
  shasum -a 256 "${dist_dir}/${asset}" >"${dist_dir}/${asset}.sha256"
fi

printf '%s\n' "${dist_dir}/${asset}"
