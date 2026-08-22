#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
pkgbuild="$root/packaging/arch/PKGBUILD"
catalog="$root/release/variants.json"

bash -n "$pkgbuild"
# shellcheck disable=SC1090
source "$pkgbuild"

for mapping in \
  'x86_64 linux-cfetch-remote-x86_64' \
  'aarch64 linux-cfetch-remote-arm64'
do
  read -r carch expected <<<"$mapping"
  CARCH=$carch
  actual=$(_cfetch_variant)
  test "$actual" = "$expected"
  jq -e --arg id "$actual" \
    '.variants[] | select(.id == $id and .backend == "endpoint")' \
    "$catalog" >/dev/null
done

for phase in build check; do
  declare -f "$phase" | grep -Fq 'CFETCH_VARIANT="$(_cfetch_variant)"'
done

echo PACKAGING_VARIANTS_OK
