#!/usr/bin/env bash
# Shared shell test helpers.

PASS=0
FAIL=0
CLEANUP_DIRS=()

_pass() {
  PASS=$((PASS + 1))
  echo "  PASS: $1"
}

_fail() {
  FAIL=$((FAIL + 1))
  echo "  FAIL: $1" >&2
}

_assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected '$expected', got '$actual')"
  fi
}

_assert_contains() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected to contain '$expected', got '$actual')"
  fi
}

_assert_exit() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" -eq "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected exit $expected, got $actual)"
  fi
}

_tmpdir() {
  local d
  d=$(mktemp -d)
  CLEANUP_DIRS+=("$d")
  echo "$d"
}

_mock_bin() {
  local d
  d=$(_tmpdir)
  echo "$d"
}

_cleanup() {
  for d in "${CLEANUP_DIRS[@]+"${CLEANUP_DIRS[@]}"}"; do
    rm -rf "$d"
  done
}
trap _cleanup EXIT

_test_summary() {
  echo
  echo "Summary: $PASS passed, $FAIL failed"
  if [[ "$FAIL" -gt 0 ]]; then
    exit 1
  fi
}
