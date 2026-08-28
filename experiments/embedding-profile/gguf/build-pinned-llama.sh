#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 SOURCE_DIRECTORY BUILD_DIRECTORY" >&2
  exit 2
fi

readonly llama_repository="https://github.com/ggml-org/llama.cpp.git"
readonly llama_tag="b10516"
readonly llama_revision="b95502ba9aa0eb73a2f4fc8878d7fbe6a847a0b9"
readonly source_directory="$1"
readonly build_directory="$2"

if [[ ! -d "${source_directory}/.git" ]]; then
  if [[ -e "$source_directory" ]] && [[ -n "$(find "$source_directory" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    echo "refusing to initialize non-empty source directory: $source_directory" >&2
    exit 1
  fi
  mkdir -p "$source_directory"
  git -C "$source_directory" init --quiet
  git -C "$source_directory" remote add origin "$llama_repository"
  git -C "$source_directory" fetch --depth 1 origin "refs/tags/${llama_tag}"
  git -C "$source_directory" checkout --detach FETCH_HEAD
fi

actual_revision="$(git -C "$source_directory" rev-parse HEAD)"
if [[ "$actual_revision" != "$llama_revision" ]]; then
  echo "llama.cpp revision mismatch: expected $llama_revision, found $actual_revision" >&2
  exit 1
fi

if [[ -n "$(git -C "$source_directory" status --short)" ]]; then
  echo "refusing to build a modified llama.cpp source tree" >&2
  exit 1
fi

cmake \
  -S "$source_directory" \
  -B "$build_directory" \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_SHARED_LIBS=OFF \
  -DGGML_NATIVE=ON \
  -DGGML_OPENMP=ON \
  -DGGML_CPU_REPACK=ON \
  -DGGML_BLAS=OFF \
  -DGGML_CUDA=OFF \
  -DGGML_HIP=OFF \
  -DGGML_SYCL=OFF \
  -DGGML_VULKAN=OFF \
  -DLLAMA_BUILD_TESTS=OFF \
  -DLLAMA_BUILD_SERVER=OFF \
  -DLLAMA_BUILD_EXAMPLES=ON

cmake \
  --build "$build_directory" \
  --config Release \
  --target llama-embedding \
  --parallel "${CFETCH_GGUF_BUILD_JOBS:-2}"

readonly executable="${build_directory}/bin/llama-embedding"
if [[ ! -x "$executable" ]]; then
  echo "build did not produce $executable" >&2
  exit 1
fi

version="$($executable --version 2>&1)"
if [[ "$version" != *"commit ${llama_revision:0:7}"* ]]; then
  echo "llama-embedding version does not attest the pinned revision" >&2
  echo "$version" >&2
  exit 1
fi

printf '%s\n' "$version"
