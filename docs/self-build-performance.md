# Self-build performance

Distributed binaries are built on the portable `release` baseline: thin LTO,
default codegen units, and no `target-cpu` pinning. That is deliberate — one
archive per platform must run on every CPU of that architecture, from the
oldest supported x86-64 to whatever ships next year. Portability costs peak
codegen, and this page is about paying that cost only when the binary never
leaves the machine it was built on.

## When this matters

Scan-heavy workloads: large brains, big `code_roots`, repeated `cfetch map`
during hook-driven sessions, full re-indexes. The time there goes to
tree-sitter parsing and SQLite writes — exactly the code fat LTO and single
codegen units help most. Expect single-digit to low-double-digit percent on
those paths, not a transformation.

Query paths (`recall`, `find`, `expand`) are I/O- and index-bound; a faster
binary moves them barely at all. Semantic recall performance is owned by the
local-inference track ([local-inference.md](local-inference.md)) — an admitted
NPU/GPU backend or a local model server does more for embeddings than any
compiler flag.

## Building

```sh
cargo build --profile release-max --locked
# binary at target/release-max/cfetch
```

The profile (`[profile.release-max]` in Cargo.toml) inherits `release` and
adds `lto = "fat"` and `codegen-units = 1`. It changes nothing about behavior
— same binary semantics, slower build, best possible codegen for one machine.

For a binary that is genuinely machine-local, also let the compiler target
the exact CPU (a `RUSTFLAGS` setting — profiles cannot express it):

```sh
RUSTFLAGS="-C target-cpu=native" cargo build --profile release-max --locked
```

PowerShell:

```powershell
$env:RUSTFLAGS = "-C target-cpu=native"
cargo build --profile release-max --locked
```

## Caveats

- `target-cpu=native` produces a binary that can crash with illegal
  instructions on any other machine. Do not copy it around; do not ship it.
  If a build might leave the machine, drop the flag and keep the profile.
- Release CI is unchanged and keeps producing portable archives; this profile
  exists for self-hosting, not for the release matrix.
- Fat LTO over this dependency tree is noticeably slower to link than the
  default. That cost is paid once per build.
