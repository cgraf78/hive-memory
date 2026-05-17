#!/usr/bin/env bash
set -euo pipefail

# Cargo.toml is the release-version source of truth. Keep version extraction in
# one tiny helper so packaging, release validation, and local tag creation cannot
# drift into subtly different parsers.
awk -F'"' '/^version =/ { print $2; exit }' Cargo.toml
