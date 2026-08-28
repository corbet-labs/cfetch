#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 BUNDLE_DIRECTORY" >&2
  exit 2
fi

readonly artifact_repo="valindotai/embeddinggemma-300m-coreml"
readonly artifact_revision="d1dc305086782e958f91fa278de97e4af9caeaf0"
readonly bundle_directory="$1"
readonly base_url="https://huggingface.co/${artifact_repo}/resolve/${artifact_revision}"

mkdir -p "$bundle_directory"

while read -r expected_sha256 relative_path; do
  [[ -n "$relative_path" ]] || continue
  destination="${bundle_directory}/${relative_path}"
  mkdir -p "$(dirname "$destination")"

  if [[ -f "$destination" ]] && \
      printf '%s  %s\n' "$expected_sha256" "$destination" | shasum -a 256 --check --status; then
    continue
  fi

  partial="${destination}.partial"
  rm -f "$partial"
  curl \
    --fail \
    --location \
    --retry 5 \
    --retry-delay 2 \
    --show-error \
    --silent \
    "${base_url}/${relative_path}?download=true" \
    --output "$partial"
  printf '%s  %s\n' "$expected_sha256" "$partial" | shasum -a 256 --check
  mv -f "$partial" "$destination"
done <<'ARTIFACT_FILES'
f8a05fa44f8f7e429a16a3855571fae45e8d5510bd7921246afb3535971fccef encoder.mlmodelc/analytics/coremldata.bin
afe90940714a33c4fdc1e07c317c971709c409684d228f3294552a6cb99c0e1c encoder.mlmodelc/coremldata.bin
28e65e1720137d53181bc378a6288cda04cba50fd446e12b75096d8b0aa24344 encoder.mlmodelc/metadata.json
48386f767d0bd5f74a935792586078acf1ab36d97369c4affafcf64adfd452bb encoder.mlmodelc/model.mil
62e84aaaa99bc7950668742301eaadb0b1a23204b5b9204dfaf20bfdd02bdf9d encoder.mlmodelc/weights/weight.bin
8f863f76e2d9c710cc833dc92efa898c9adfd41031c786507cc6b0e49c2e3e68 hf_model/config.json
2f7b0adf4fb469770bb1490e3e35df87b1dc578246c5e7e6fc76ecf33213a397 hf_model/special_tokens_map.json
6852f8d561078cc0cebe70ca03c5bfdd0d60a45f9d2e0e1e4cc05b68e9ec329e hf_model/tokenizer.json
1299c11d7cf632ef3b4e11937501358ada021bbdf7c47638d13c0ee982f2e79c hf_model/tokenizer.model
9076840490613047bc9115963ee96b7702018b0d26ba644240bf856efda93118 hf_model/tokenizer_config.json
53030df6aa4cbab4daddfdce913622f4b91604541e4683ed289bc30fc822dda6 model_config.json
ARTIFACT_FILES
