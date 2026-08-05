# shellcheck shell=bash
#
# Hive Memory-specific runtime assertions for the shared smoke-release.sh.
#
# The shared script already checks archive naming, the executable bit, and that
# every declared payload entry shipped. This adds the checks that require
# actually running the artifact, so it is skipped for cross-built android-*
# archives that cannot execute on the runner.

release_smoke_check() {
  local root=$1
  local store config rc=0

  "$root/hm" --version

  # Exercise a real store lifecycle rather than just --version: an archive can
  # start up fine and still be missing the runtime behavior consumers bootstrap
  # against. Keep the fixtures out of the release checkout.
  store=$(mktemp -d)
  config=$(mktemp)

  "$root/hm" stores init personal --root "$store" || rc=$?

  if [[ "$rc" -eq 0 ]]; then
    cat >"$config" <<EOF
default_store = "personal"

[stores.personal]
root = "$store"
EOF
    "$root/hm" --config "$config" doctor --quick || rc=$?
  fi

  rm -rf "$store" "$config"
  return "$rc"
}
