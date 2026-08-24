#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 4 || $# -gt 5 ]]; then
  echo "usage: $0 BUNDLE BUNDLE_SHA256 CFETCH_BINARY REPORT_JSON [PROVIDER]" >&2
  exit 2
fi

bundle=$1
expected_bundle_sha256=$2
cfetch_binary=$3
report=$4
provider=${5:-auto}

[[ $expected_bundle_sha256 =~ ^[0-9a-f]{64}$ ]] || {
  echo "bundle SHA-256 must be 64 lowercase hexadecimal characters" >&2
  exit 2
}
[[ -f $bundle ]] || { echo "model bundle not found: $bundle" >&2; exit 2; }
[[ -x $cfetch_binary ]] || { echo "cfetch binary is not executable: $cfetch_binary" >&2; exit 2; }

if command -v sha256sum >/dev/null; then
  actual_bundle_sha256=$(sha256sum "$bundle" | awk '{print $1}')
else
  actual_bundle_sha256=$(shasum -a 256 "$bundle" | awk '{print $1}')
fi
[[ $actual_bundle_sha256 == "$expected_bundle_sha256" ]] || {
  echo "model bundle SHA-256 $actual_bundle_sha256 does not match $expected_bundle_sha256" >&2
  exit 1
}

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

# Only regular files/directories under the one frozen top-level directory are
# admitted. This deliberately rejects links and device entries before tar sees
# the host filesystem.
model_dir=$(python3 - "$bundle" "$work" <<'PY'
import pathlib
import sys
import tarfile

bundle = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2]).resolve()
expected_root = "cfetch-embeddinggemma-300m-a8w8-v1"
with tarfile.open(bundle, "r:gz") as archive:
    members = archive.getmembers()
    if not members:
        raise SystemExit("model bundle is empty")
    for member in members:
        path = pathlib.PurePosixPath(member.name)
        if path.is_absolute() or ".." in path.parts:
            raise SystemExit(f"unsafe model bundle path: {member.name!r}")
        if not path.parts or path.parts[0] != expected_root:
            raise SystemExit(f"unexpected model bundle root: {member.name!r}")
        if not (member.isdir() or member.isreg()):
            raise SystemExit(f"unsupported model bundle entry: {member.name!r}")
        target = (destination / pathlib.Path(*path.parts)).resolve()
        if destination not in target.parents and target != destination:
            raise SystemExit(f"escaping model bundle path: {member.name!r}")
    archive.extractall(destination, members=members, filter="data")
print(destination / expected_root)
PY
)

(
  cd "$model_dir"
  if command -v sha256sum >/dev/null; then
    sha256sum --check SHA256SUMS
  else
    shasum -a 256 --check SHA256SUMS
  fi
)

"$cfetch_binary" inference-certify \
  --model-dir "$model_dir" \
  --provider "$provider" \
  --json > "$report"

jq -e '
  .schema == 2 and
  .profile_id == "cfetch-embedding-v1" and
  .profile_manifest_sha256 == "3a7645ee84a5fe21bf0befaf6b68f51a5ff61ad22b0c10c40aaec0a1f63d7a53" and
  .artifact_sha256 == "ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1" and
  .ort_deterministic_compute == true and
  .ort_precise_qmm == true and
  .exact_vector_conformance == true and
  (.provider != "cpu" or .producer_eligible_without_external_review == true) and
  (.known_answers | length == 11) and
  all(.known_answers[]; .passed == true)
' "$report" >/dev/null

echo "CFETCH_INFERENCE_CERTIFICATION_OK provider=$provider report=$report"
