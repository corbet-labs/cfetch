# Local accelerated inference

cfetch's target is local accelerated inference on every supported install.
The runtime order is fixed:

1. NPU
2. GPU
3. accelerated CPU

An NPU is preferred whenever an admitted NPU backend initializes and owns the
required graph. If it cannot run, cfetch tries the best admitted GPU backend;
if that cannot run, it uses the accelerated CPU floor. Remote inference is an
explicit deployment choice, not the automatic fallback and not a substitute
for a missing local implementation.

## Backend plan

Different hardware families need different native runtimes and artifacts.
That is compatible with one vector space because the model semantics and
mixed-backend quality gate are shared.

| Device | Primary runtime direction | Native artifact direction |
|---|---|---|
| Apple Neural Engine | Core ML | mlpackage / compiled model |
| Intel NPU | OpenVINO | OpenVINO IR / compiled blob |
| AMD NPU | Ryzen AI / Vitis AI | vendor-compiled graph |
| Qualcomm NPU | LiteRT with Qualcomm delegate or QNN | TFLite / QNN context binary |
| NVIDIA GPU | TensorRT | serialized INT8 engine |
| AMD GPU | MIGraphX / ROCm | compiled ONNX graph |
| Intel GPU | OpenVINO | OpenVINO IR / compiled cache |
| Apple GPU | Core ML / Metal | compiled Core ML graph |
| Windows GPU floor | DirectML | compiled ONNX graph |
| CPU floor | XNNPACK, oneDNN, or equivalent SIMD runtime | runtime-native INT8 graph |

The table is a build plan, not a support claim. Current admission state is
recorded in release/inference-backends.json. A runtime is not shipped merely
because it loads a model or detects a device.

## What admission proves

Every local backend must provide:

- a pinned source revision and reproducible native artifact manifest;
- verified placement on the claimed NPU, GPU, or accelerated CPU kernels;
- finite, non-zero 768-dimensional outputs;
- byte-repeatability on the same runtime/artifact/device;
- the fixed NPU-anchor quality floor;
- the complete NPU/GPU/CPU query-document retrieval matrix;
- latency and peak-memory measurements for the supported sequence buckets.

Backend admission is scoped to the artifact, runtime, device family, and
compiler settings in its manifest. A different NPU family may emit slightly
different records and must run the same matrix before release.

## Selection and failure

Hardware discovery supplies candidates in NPU, GPU, CPU order. Runtime
initialization proves whether a candidate is usable. Selection never promotes
a detected device on name alone and never accepts silent CPU remainder while
claiming NPU or GPU acceleration.

Failure is local and ordered:

    admitted NPU fails -> admitted GPU -> accelerated CPU

Remote use happens only when the operator explicitly configures the endpoint
transport. It is not inserted into that local chain.

## Current state

The public release catalog is still endpoint-only. The previous local ORT
candidate and exact-byte certification command were removed because they
encoded the rejected CPU-oracle architecture. Native local packages return to
the catalog only after an NPU anchor plus GPU and CPU artifacts pass the new
gate.

The immediate implementation order is:

1. build and measure the first native NPU artifact;
2. export its pinned SciFact vectors and establish the NPU anchor;
3. build accelerated GPU and CPU artifacts from the same source revision;
4. run every mixed query/document pairing;
5. integrate runtime selection and publish local variants.
