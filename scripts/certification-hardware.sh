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
    if command -v lspci >/dev/null 2>&1; then
      lspci -nn \
        | sed -n -E '/(VGA compatible controller|3D controller|Display controller|Processing accelerators|Co-processor)/Ip' \
        | sed -E 's/^[[:xdigit:]:.]+[[:space:]]+/pci_device=/'
    fi
    if command -v rocminfo >/dev/null 2>&1; then
      # Only public device-class fields are selected. In particular, never
      # copy rocminfo's UUID, unique ID, node ID, BDF, cache or memory detail.
      rocminfo 2>/dev/null \
        | sed -n -E \
          -e '/^[[:space:]]+Name:[[:space:]]+gfx[[:alnum:]]+[[:space:]]*$/p' \
          -e '/^[[:space:]]+Marketing Name:[[:space:]]+AMD (Radeon|Instinct)[[:space:]]/p' \
          -e '/^[[:space:]]+Device Type:[[:space:]]+GPU[[:space:]]*$/p' \
          -e '/^[[:space:]]+Wavefront Size:/p' \
        | sed -E 's/^[[:space:]]+/rocm_/'
    fi
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
