#!/usr/bin/env bash
set -euo pipefail

printf 'kernel=%s\n' "$(uname -s)"
printf 'architecture=%s\n' "$(uname -m)"
printf 'kernel_release=%s\n' "$(uname -r)"

if [[ -n ${ImageOS:-} ]]; then
  printf 'github_image_os=%s\n' "$ImageOS"
fi
if [[ -n ${ImageVersion:-} ]]; then
  printf 'github_image_version=%s\n' "$ImageVersion"
fi

case "$(uname -s)" in
  Linux)
    lscpu | sed -n \
      -e '/^Architecture:/p' \
      -e '/^Vendor ID:/p' \
      -e '/^Model name:/p' \
      -e '/^CPU family:/p' \
      -e '/^Model:/p' \
      -e '/^Stepping:/p' \
      -e '/^Flags:/p' \
      -e '/^Hypervisor vendor:/p' \
      -e '/^Virtualization type:/p'
    ;;
  Darwin)
    for key in hw.model hw.ncpu hw.optional.arm64 machdep.cpu.brand_string; do
      value=$(sysctl -n "$key" 2>/dev/null || true)
      if [[ -n $value ]]; then
        printf '%s=%s\n' "$key" "$value"
      fi
    done
    ;;
esac
