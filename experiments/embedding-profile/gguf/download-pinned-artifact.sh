#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 OUTPUT_FILE" >&2
  exit 2
fi

readonly artifact_repository="ggml-org/embeddinggemma-300M-GGUF"
readonly artifact_revision="0f741b5a6585bd53aeb15cd1372c56f2a0f65e12"
readonly artifact_file="embeddinggemma-300M-Q8_0.gguf"
readonly artifact_sha256="b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63"
readonly output_file="$1"
readonly artifact_url="https://huggingface.co/${artifact_repository}/resolve/${artifact_revision}/${artifact_file}?download=true"

mkdir -p "$(dirname "$output_file")"

if [[ -f "$output_file" ]] && \
    printf '%s  %s\n' "$artifact_sha256" "$output_file" | sha256sum --check --status; then
  exit 0
fi

readonly partial_file="${output_file}.partial"
rm -f "$partial_file"
trap 'rm -f "$partial_file"' EXIT

curl \
  --fail \
  --location \
  --retry 5 \
  --retry-delay 2 \
  --show-error \
  --silent \
  "$artifact_url" \
  --output "$partial_file"

printf '%s  %s\n' "$artifact_sha256" "$partial_file" | sha256sum --check
mv -f "$partial_file" "$output_file"
trap - EXIT
