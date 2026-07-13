#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. Ensure the NDK-built Android
# binary starts successfully inside the real Termux app sandbox.
.termux-ci/hm --help
