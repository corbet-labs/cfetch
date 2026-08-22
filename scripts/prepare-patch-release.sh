#!/usr/bin/env bash
# Prepare exactly the next pre-1.0 patch release. Safe to rerun when a release
# commit exists but CI stopped before its tag was created.
set -euo pipefail

[[ -z "$(git status --short --untracked-files=no)" ]] || {
  echo "release preparation requires a clean tracked worktree" >&2
  exit 1
}

current=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
latest_tag=$(git tag --list 'v[0-9]*' --sort=-v:refname | head -1)
latest=${latest_tag#v}
[[ -n "$current" && -n "$latest" ]] || {
  echo "cannot resolve Cargo version or latest release tag" >&2
  exit 1
}

IFS=. read -r major minor patch <<< "$current"
[[ "$major" == 0 && "$minor" =~ ^[0-9]+$ && "$patch" =~ ^[0-9]+$ ]] || {
  echo "cfetch 1.0+ is operator-blocked (current version $current)" >&2
  exit 1
}

if [[ "$current" == "$latest" ]]; then
  version="$major.$minor.$((patch + 1))"
  sed -i "0,/^version = \"$current\"/s//version = \"$version\"/" Cargo.toml
  sed -i "s/^pkgver=$current\$/pkgver=$version/" packaging/arch/PKGBUILD
  sed -i "s/^[[:space:]]*pkgver = $current\$/\tpkgver = $version/" packaging/arch/.SRCINFO
  sed -i "s/#tag=v$current\$/#tag=v$version/" packaging/arch/.SRCINFO
  OLD="$current" NEW="$version" perl -0pi \
    -e 's/(\[\[package\]\]\nname = "cfetch"\nversion = ")\Q$ENV{OLD}\E("\n)/$1$ENV{NEW}$2/' \
    Cargo.lock
  NEW="$version" perl -0pi -e 's/## Unreleased\n/## Unreleased\n\n## $ENV{NEW}\n/' CHANGELOG.md
elif ! git rev-parse -q --verify "refs/tags/v$current" >/dev/null && grep -q "^## $current\$" CHANGELOG.md; then
  # A prior automation run committed the bump, but its CI failed before tag.
  version=$current
else
  echo "Cargo.toml $current is neither latest tag $latest_tag nor an untagged prepared release" >&2
  exit 1
fi

case "$version" in
  0.*) ;;
  *) echo "cfetch 1.0+ is operator-blocked (prepared version $version)" >&2; exit 1 ;;
esac

grep -q "^version = \"$version\"\$" Cargo.toml
grep -q "^pkgver=$version\$" packaging/arch/PKGBUILD
grep -q "^[[:space:]]*pkgver = $version\$" packaging/arch/.SRCINFO
grep -q "#tag=v$version\$" packaging/arch/.SRCINFO
grep -A2 '^name = "cfetch"$' Cargo.lock | grep -q "^version = \"$version\"\$"
printf '%s\n' "$version"
