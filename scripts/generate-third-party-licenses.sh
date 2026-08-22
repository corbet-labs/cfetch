#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
output=${1:-"$repo_root/THIRD-PARTY-LICENSES.txt"}
generated=$(mktemp)
metadata=$(mktemp)
assembled=$(mktemp)
trap 'rm -f "$generated" "$metadata" "$assembled"' EXIT

cd "$repo_root"
cargo about generate --locked --fail -c "$repo_root/about.toml" \
  "$repo_root/about.hbs" -o "$generated"
cargo metadata --locked --format-version 1 >"$metadata"

# cargo-about gathers license texts and copyright notices, but Apache NOTICE
# files are separate legal artifacts. Append every NOTICE shipped by a locked
# package so adding one cannot silently omit its attribution requirements.
{
  cat "$generated"
  printf '\n\nApache NOTICE files\n===================\n'
  jq -r '.packages[] | [.name, .version, .manifest_path] | @tsv' "$metadata" \
    | LC_ALL=C sort -t $'\t' -k1,1 -k2,2 \
    | while IFS=$'\t' read -r crate version manifest; do
        crate_dir=${manifest%/Cargo.toml}
        while IFS= read -r -d '' notice; do
          printf '\n--------------------------------------------------------------------------------\n'
          printf '%s %s - %s\n\n' "$crate" "$version" "${notice##*/}"
          cat "$notice"
          printf '\n'
        done < <(find "$crate_dir" -maxdepth 1 -type f -iname 'NOTICE*' -print0 | LC_ALL=C sort -z)
      done
} >"$assembled"

# Registry license files are a mix of LF/CRLF and occasionally carry trailing
# spaces. Normalize whitespace without changing any license wording, and omit
# surplus blank lines at EOF so the checked-in artifact stays diff-clean.
awk '
  {
    sub(/\r$/, "")
    sub(/[ \t]+$/, "")
    if (length($0) == 0) { blank++; next }
    while (blank > 0) { print ""; blank-- }
    print
  }
' "$assembled" >"$output"
