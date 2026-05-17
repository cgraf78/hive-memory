#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/release.sh [--push]

Create the release tag for the version in Cargo.toml.

Options:
  --push   push main and the generated tag to origin
EOF
}

push=0
case "${1:-}" in
  "")
    ;;
  --push)
    push=1
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
esac

if [[ $# -gt 1 ]]; then
  usage
  exit 2
fi

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

if [[ $(git branch --show-current) != "main" ]]; then
  echo "release: run from main" >&2
  exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "release: worktree must be clean" >&2
  exit 1
fi

version=$(scripts/cargo-version.sh)
tag="v${version}"

# The version appears in the Git tag because GitHub releases are tag-based, but
# humans should not type it twice. Derive the tag from Cargo.toml and make the
# workflow verify that the pushed tag still matches the crate version.
#
# A new release must use a new version. Check origin before creating or pushing
# anything so rerunning the helper cannot accidentally replace assets for an
# already published version.
git fetch --quiet origin main:refs/remotes/origin/main --tags
if git rev-parse --verify origin/main >/dev/null 2>&1 &&
  ! git merge-base --is-ancestor origin/main HEAD; then
  echo "release: local main is not based on origin/main; update before releasing" >&2
  exit 1
fi
if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
  echo "release: origin already has tag ${tag}; bump Cargo.toml before releasing" >&2
  exit 1
fi
if command -v gh >/dev/null 2>&1 && gh release view "$tag" >/dev/null 2>&1; then
  echo "release: GitHub release ${tag} already exists; bump Cargo.toml before releasing" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  tagged_commit=$(git rev-list -n 1 "$tag")
  head_commit=$(git rev-parse HEAD)
  if [[ "$tagged_commit" != "$head_commit" ]]; then
    echo "release: tag ${tag} already points at ${tagged_commit}, not HEAD ${head_commit}" >&2
    exit 1
  fi
  echo "release: tag ${tag} already exists at HEAD"
else
  git tag -a "$tag" -m "$tag"
  echo "release: created tag ${tag}"
fi

if [[ "$push" == 1 ]]; then
  git push origin main
  git push origin "$tag"
fi

echo "release: ${tag}"
