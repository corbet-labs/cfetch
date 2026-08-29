# Intel OpenVINO target package

Status: build and physical-probe tooling, not an admitted backend.

This directory produces the first cfetch target package for one exactly
identified Intel NPU, GPU, and accelerated CPU cohort. The three scopes share
one converted EmbeddingGemma graph but are compiled, executed, and evidenced
independently. This is not a generic-Intel package and it does not activate a
scope in the shared vector space.

The package keeps these boundaries explicit:

- cfetch's portable boundary is signed `INT8x768`. OpenVINO executes the
  target-native internal precision recorded by each scope; internal INT8 is
  not assumed. Current Intel NPU documentation states that internal hardware
  computation can remain FP16 for quantized or mixed graphs.
- the IR contains the pinned transformer, attention-mask-weighted mean pooling
  including the supplied prompt, the bias-free 768 -> 3072 and 3072 -> 768
  identity projections, and L2 normalization.
- each OpenVINO model is reshaped and compiled statically at 32, 64, 128, 256,
  512, 1,024, or 2,048 tokens. Input over 2,048 tokens is rejected; it is never
  truncated.
- a request names a package-admitted scope ID. The manifest alone maps that ID
  to exactly `NPU`, `GPU`, or `CPU`; `AUTO`, `MULTI`, `HETERO`, and implicit
  cross-device fallback are forbidden.
- NPU initialization remains lazy. A generic HTTP 503
  `scope_unavailable` response lets cfetch try the next independently admitted
  GPU or CPU scope without the adapter claiming where a failed request ran.

## Package states

The top-level `package_state` prevents the evidence bootstrap cycle:

| State | Three per-scope physical evidence digests | Compatibility report |
| --- | --- | --- |
| `physical-probe` | all explicitly `null` | `null` |
| `candidate` | all required | `null` |
| `release` | all required | required |

Only `physical-probe` may be used to collect the initial live device evidence.
It is deliberately unreleasable. The collector retains the probe package
manifest digest. Candidate construction may fill only the three evidence
bindings and state; release construction additionally fills the global report
binding. Admission tooling must reject pending bindings and prove that graph,
runtime, device, host, key, and all other immutable fields did not change.

## Reproducible build inputs

The manual `OpenVINO pinned package inputs` GitHub Actions workflow is the
supported build entry point. It runs on Ubuntu 22.04 x86_64 with CPython 3.12
and an exact glibc 2.35 build floor. It:

1. installs only binary wheels from `requirements-build.lock` with
   `--require-hashes`, using PyPI plus the official PyTorch CPU wheel index;
2. fetches the public immutable mirror `unsloth/embeddinggemma-300m` commit
   `bfa3c846ac738e62aa61806ef9112d34acb1dc5a`, whose 13 required files are
   byte-identical to the frozen `google/embeddinggemma-300m` commit
   `57c266a740f537b4dc058e1b0cda161fd15afa75`, and verifies every file against
   the canonical SHA-256 allowlist without sending credentials;
3. fetches, extracts, and hash-checks the pinned official Gemma Terms and
   Prohibited Use Policy;
4. converts the exact graph and runs a real CPU PyTorch-to-OpenVINO parity
   smoke at the 32, 128, and 2,048 static buckets;
5. freezes the runtime, executes its integrity launcher and CPU plugin
   self-check, and emits regular-file-only content-addressed archives.

The fetch report and artifact manifest record both the canonical Google source
identity and the exact public acquisition commit. The mirror is transport,
not a new model identity: a single differing byte aborts the build. The parity
report is a broken-export smoke check, not compatibility admission evidence.

The equivalent local conversion commands, after obtaining the exact source
under the Gemma terms, are:

```console
python packages/openvino/legal.py fetch --output-dir "$LEGAL_DIR"
python packages/openvino/fetch_source.py \
  --output-dir "$PINNED_SOURCE_DIR" \
  --cache-dir "$HF_CACHE_DIR"
python packages/openvino/convert.py \
  --source-dir "$PINNED_SOURCE_DIR" \
  --legal-dir "$LEGAL_DIR" \
  --output-dir "$ARTIFACT_DIR" \
  --weight-storage f16
python packages/openvino/smoke_parity.py \
  --source-dir "$PINNED_SOURCE_DIR" \
  --artifact-dir "$ARTIFACT_DIR" \
  --output "$PARITY_REPORT"
```

`f16` compresses constants stored in the IR; it is not a cross-device
arithmetic claim. The converter verifies all source and semantic configuration
hashes before loading weights and proves that all seven required static shapes
can be formed before serialization.

## Gemma redistribution boundary

The converted IR is a Gemma Model Derivative. A distributable artifact must
contain all five exact files below, and `archive.py --require-gemma-legal`
fails closed if any byte differs:

- `GEMMA_TERMS.txt`
- `GEMMA_PROHIBITED_USE_POLICY.txt`
- `MODEL_USE_RESTRICTIONS.txt`
- `MODEL_MODIFICATIONS.txt`
- `NOTICE`

The payload includes the full pinned Agreement and Prohibited Use Policy, an
enforceable Section 3.2 pass-through restriction, prominent conversion
modification notice, and Google's mandated NOTICE sentence. Do not publish
weights or converted IR without this payload and the required downstream use
agreement.

## Frozen runtime and integrity launcher

Build the target runtime with:

```console
python packages/openvino/build_runtime.py \
  --output-dir "$RUNTIME_DIR" \
  --minimum-glibc 2.35 \
  --cc /usr/bin/cc
```

The result contains a native root executable `cfetch-openvino-adapter`, a
PyInstaller-frozen `cfetch-openvino-adapter-runtime`, CPython, and the exact
packaged `cryptography`, `numpy`, `openvino`, and `tokenizers` dependencies. It
does not depend on a target host's system Python.

Final assembly inventories every runtime, interpreter, native-library,
adapter, manifest, graph, tokenizer, legal, and key file. The inventory digest
is patched into the native root launcher; the launcher is the sibling binary
whose SHA-256 cfetch binds. Before Python or OpenVINO starts, that launcher
rejects missing, extra, symlinked, mode-changed, size-changed, or digest-changed
files. The frozen adapter verifies the same inventory again.

This relocation claim is intentionally narrow: Linux x86_64 with glibc 2.35 or
newer. Kernel drivers, firmware, Level Zero/OpenCL user-mode drivers, and
admitted CPU instruction support remain host prerequisites and are not
vendored. Each scope therefore binds exact OpenVINO properties, the observed
`EXECUTION_DEVICES`, kernel release, and hashes of the relevant regular driver
libraries. A driver or kernel change requires recertification.

OpenVINO IR is shipped instead of a compiled blob because compiled blobs are
not stable across OpenVINO/device versions. The exact runtime compiles the IR
on the admitted host; any generated cache is disposable.

## Probe package assembly

A scope configuration contains the top-level state, exact frozen runtime
versions, and three ordered NPU/GPU/CPU entries. A physical-probe NPU entry has
this shape; angle-bracket values are intentionally not usable evidence:

```json
{
  "schema_version": 1,
  "package_state": "physical-probe",
  "dependency_versions": {
    "cryptography": "<exact>",
    "numpy": "<exact>",
    "openvino": "<exact>",
    "tokenizers": "<exact>"
  },
  "scopes": [
    {
      "scope_id": "<exact-intel-npu-scope>",
      "backend": "openvino",
      "transport": "supervised-local",
      "runtime": "<exact-runtime-identity>",
      "compiler": "<exact-compiler-and-settings-identity>",
      "package_target": "linux-x86_64-glibc2.35",
      "artifact_source": "google/embeddinggemma-300m@57c266a740f537b4dc058e1b0cda161fd15afa75",
      "artifact_sha256": "<filled-by-assemble.py>",
      "internal_precision": "fp16-hardware-compute",
      "device_class": "npu",
      "device": "<exact-device-family>",
      "openvino_device": "NPU",
      "openvino_compile_config": {},
      "required_openvino_properties": {
        "FULL_DEVICE_NAME": "<observed>",
        "DEVICE_ARCHITECTURE": "<observed>",
        "NPU_DRIVER_VERSION": 0,
        "NPU_COMPILER_VERSION": 0
      },
      "required_execution_devices": ["NPU"],
      "required_host": {
        "system": "Linux",
        "machine": "x86_64",
        "kernel_release": "<observed>",
        "files": [
          {"path": "/usr/lib/<exact-driver-library>", "sha256": "<sha256>"}
        ]
      },
      "placement_evidence_sha256": null,
      "supported_max_tokens": 2048,
      "supported_sequence_buckets": [32, 64, 128, 256, 512, 1024, 2048],
      "supported_max_batch_size": 64,
      "sequence_capability_evidence_sha256": null,
      "performance_evidence_sha256": null,
      "compatibility_report_sha256": null,
      "attestation_public_key": "<64-lowercase-hex>",
      "attestation_private_key_file": "<input-key-file>",
      "accelerated_placement": true
    }
  ]
}
```

GPU requires exact `FULL_DEVICE_NAME`, `DEVICE_ARCHITECTURE`,
`GPU_UARCH_VERSION`, and `GPU_DEVICE_ID`; CPU requires exact
`FULL_DEVICE_NAME` and `DEVICE_ARCHITECTURE`. The host-file bindings cover
driver identity that OpenVINO does not expose as a supported device property.
All three classes and a distinct Ed25519 key per scope are mandatory.

Assemble and self-check the final directory with:

```console
python packages/openvino/assemble.py \
  --artifact-dir "$ARTIFACT_DIR" \
  --runtime-dir "$RUNTIME_DIR" \
  --runtime-manifest-sha256 "$RUNTIME_MANIFEST_SHA256" \
  --scope-config "$SCOPE_CONFIG" \
  --output-dir "$PACKAGE_DIR"
```

`assemble.py` never creates keys, invents evidence, or silently chooses a
device. Its JSON result reports the final launcher, launcher digest, runtime
manifest digest, and package inventory digest.

## Supervisor and signed HTTP contract

The cfetch parent invokes the packaged sibling directly:

```console
./cfetch-openvino-adapter serve --host 127.0.0.1 --port 0 --auth-stdin
```

It writes exactly one JSON line containing a fresh 32-byte lowercase-hex bearer
and keeps the pipe open:

```json
{"bearer":"<64-lowercase-hex>"}
```

After package/runtime integrity checks, the child emits exactly one bounded
readiness line to stdout:

```json
{"schema_version":1,"url":"http://127.0.0.1:<ephemeral>/v1","scope_ids":["<npu>","<gpu>","<cpu>"]}
```

EOF on stdin shuts it down. Diagnostics go to stderr. HTTP accepts only
authenticated `POST /v1/embeddings` with a fresh
`X-Cfetch-Attestation-Nonce` and this exact body shape:

```json
{
  "model": "google/embeddinggemma-300m",
  "dimensions": 768,
  "input": ["already-prefixed text"],
  "cfetch_requested_scope_id": "<selected-scope>"
}
```

The signed response carries `cfetch_execution`, including `package_state`,
`transport: "supervised-local"`, and all four explicit evidence/report
bindings. It also carries live `cfetch_runtime_evidence` with exact host
identity, host-file hashes, required OpenVINO properties, and one record per
executed bucket. Placement comes from
`compiled_model.get_property(EXECUTION_DEVICES)` and device properties come
from `core.get_property`; echoed request labels are not placement evidence.

The packaged Ed25519 private keys are distributed bytes. Their signatures bind
the nonce, exact request body, and exact response body inside the
supervisor-controlled local process; they are not proof of remote identity or
secret package possession. Remote service scopes require a separate
`remote-attested` transport and a non-distributed operator key.

## Remaining physical work

The manual build must first reproduce the canonical bytes from the pinned
public mirror. Then an exact `physical-probe` package must run on the target
Intel cohort. The external
physical collector—not this build recipe—must retain signed raw transactions
covering all seven buckets, live placement/properties/host identity, latency,
RSS, 1-through-64 wire grouping, output digests, and repeatability. Energy may
be explicitly unmeasured; it must not be synthesized. Only the global
all-pairs compatibility evaluation and final published-package replay can
produce candidate/release bindings.

Run dependency-light checks with:

```console
python -m unittest discover -s packages/openvino/tests -v
```
