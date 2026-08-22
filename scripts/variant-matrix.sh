#!/usr/bin/env bash
set -euo pipefail

catalog=${1:-release/variants.json}

# This is the no-compile gate used by both CI and the tag workflow. The Rust
# parser enforces the same product rules inside the executable; this shell
# boundary exists so a malformed catalog cannot even create a build matrix.
jq -e '
  .schema_version == 1 and
  (.variants | length > 0) and
  ([.variants[].id] | length == (unique | length)) and
  all(.variants[];
    (.id | test("^[a-z0-9_-]+$")) and
    (.id | contains("-cfetch-remote-")) and
    (.os == "linux" or .os == "mac" or .os == "win") and
    (.arch == "x86_64" or .arch == "aarch64") and
    (.runner | length > 0) and
    (.binary | length > 0) and
    (.archive == "tar.gz" or .archive == "zip") and
    .backend == "endpoint" and
    .cargo_features == "")
' "$catalog" >/dev/null

jq -c '{include: .variants}' "$catalog"
