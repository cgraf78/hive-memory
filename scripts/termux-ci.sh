#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# Run the suite natively under Android/Bionic and ensure the user-facing binary
# starts successfully in Termux's application sandbox.
pkg update -y
pkg install -y git rust
export CARGO_BUILD_JOBS=1
cargo test --locked
cargo run --locked --bin hm -- --help
