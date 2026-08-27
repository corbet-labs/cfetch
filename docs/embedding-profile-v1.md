# Embedding profile v1

Status: candidate, not activated by a released local producer.

cfetch uses one shared 768-dimensional INT8 vector space. The source model and
semantic pipeline are fixed. The executable graph is not: an NPU, GPU, or CPU
may need a different native artifact to execute the same model efficiently.

Run cfetch embedding-profile --json to print the machine-readable contract.

## Fixed vector-space contract

| Field | Value |
|---|---|
| Profile | cfetch-embedding-v1 |
| Source model | google/embeddinggemma-300m-qat-q8_0-unquantized |
| Source revision | 7b5b24595322ab0ea4d08827066860a6df8cb0aa |
| Numeric direction | INT8 design center |
| Dimensions | 768 |
| Maximum context | 2048 tokens |
| Query prompt | task: search result \| query: |
| Document prompt | title: none \| text: |
| Pooling | attention-mask-weighted mean, including the prompt |
| Stored record | signed INT8x768, per-vector max-absolute scale with round-to-nearest-even |
| Preferred device | NPU |
| Local order | NPU, then GPU, then accelerated CPU |
| Admission | mixed query-backend/document-backend retrieval |

The tokenizer files and configuration are pinned in the executable manifest.
Changing the source revision, tokenizer, prompts, pooling, dimensions, or
stored vector codec creates a new vector space and requires re-embedding.

## Backend-native artifacts

The following are intentionally outside the shared identity:

- runtime graph serialization and container format;
- compiler/runtime version and graph cache;
- kernel fusion, scheduling, and device-specific quantization layout;
- provider-specific execution controls.

Each released backend still pins and hashes its own artifact and runtime. Those
values identify that backend's reproducible build; they do not force another
device family to consume the same file.

This separation is required for actual NPU execution. OpenVINO, Core ML,
LiteRT/QNN, TensorRT, and CPU runtimes compile and partition models
differently. Treating one ORT file as the network identity made a CPU route the
oracle and excluded valid accelerator kernels.

## Numeric compatibility

A backend must be repeatable on the same runtime, artifact, and device. Exact
bytes across different backends or different NPU families are not required.

The NPU is the reference direction. An admitted NPU artifact must first meet
the fixed SciFact quality floor. Every candidate NPU, GPU, and CPU artifact is
then tested in both roles:

| Queries | Documents |
|---|---|
| NPU | NPU |
| NPU | GPU |
| NPU | CPU |
| GPU | NPU |
| GPU | GPU |
| GPU | CPU |
| CPU | NPU |
| CPU | GPU |
| CPU | CPU |

All pairings must stay within the frozen retrieval tolerances of the NPU/NPU
anchor. Cross-backend byte equality and corresponding-vector cosine remain
diagnostics only. The executable gate lives in
experiments/embedding-profile/cross_backend_eval.py.

## Shared-store behavior

cfetch stores one canonical record per content hash. The first record derived
by an admitted backend is retained and shared; another record does not
overwrite it. This is a derive-once storage rule, not a claim that every
backend would have emitted the same bytes.

The matrix is what makes this safe: a query computed locally on any admitted
backend must retrieve correctly against documents first derived on any other
admitted backend.

## Pre-activation correction

The former candidate pinned the artifact
cfetch-embeddinggemma-300m-a8w8-v1 and required all hardware to reproduce its
ORT CPU bytes. That graph also failed the frozen retrieval-quality budget and
did not provide complete native INT8 accelerator coverage. It is retired and
is not an active cfetch vector-space artifact.

No tagged release enabled a local network-major-1 producer, and the public
variant catalog remained endpoint-only. The v1 candidate can therefore be
corrected before activation without reinterpreting a released shared store.

Activation requires at least one accelerated NPU, GPU, and CPU backend, the
complete mixed-backend matrix, native placement evidence, and reproducible
artifact/runtime manifests.
