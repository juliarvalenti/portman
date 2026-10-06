#!/usr/bin/env bash
# Cut a release: bump the version everywhere, commit, tag, and push the tag.
# GitHub Actions (.github/workflows/release.yml) then builds Portman.dmg and
# the CLI and publishes them on the release.
#
#   scripts/release.sh 0.3.0          # or: patch | minor | major
#
# The workspace Cargo.toml version is the source of truth; the Tauri app takes
# its version from Cargo, and app/package.json is kept in step.
set -euo pipefail
cd "$(dirname "$0")/.."

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
IFS=. read -r major minor patch <<<"$current"
case "${1:-}" in
  patch) next="$major.$minor.$((patch + 1))" ;;
  minor) next="$major.$((minor + 1)).0" ;;
  major) next="$((major + 1)).0.0" ;;
  [0-9]*.[0-9]*.[0-9]*) next="$1" ;;
  *) echo "usage: $0 <x.y.z | patch | minor | major>   (current: $current)" >&2; exit 1 ;;
esac
tag="v$next"

branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = main ] || { echo "release from main (on $branch)" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is not in sync with origin/main" >&2; exit 1; }
! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "$tag already exists" >&2; exit 1; }

echo "releasing $current → $next"
sed -i '' "s/^version = \"$current\"/version = \"$next\"/" Cargo.toml
(cd app && npm version "$next" --no-git-tag-version --allow-same-version >/dev/null)
cargo check -q --workspace   # refreshes Cargo.lock
cargo test -q --workspace --exclude portman-app

git add Cargo.toml Cargo.lock app/package.json app/package-lock.json
git commit -q -m "Release $tag"
git tag -a "$tag" -m "Portman $tag"
git push -q origin main "$tag"

echo "pushed $tag — watch the build: gh run watch \$(gh run list -w release -L1 --json databaseId -q '.[0].databaseId')"
