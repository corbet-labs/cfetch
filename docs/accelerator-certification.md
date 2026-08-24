# Accelerator certification

cfetch network major 1 has one W8A8 model graph and one signed `INT8x768`
record. CPU, GPU and NPU backends are alternative execution paths for those
same bytes. A provider is a producer only after evidence proves all of the
following:

1. the exact released artifact and tokenizer were admitted by digest;
2. the intended provider owned the complete graph with CPU fallback disabled;
3. its learned regions ran on INT8-capable device kernels, not dequantized
   higher-precision substitutes; and
4. all 11 public known answers matched the canonical 768-byte records exactly.

Device discovery, successful compilation, an “INT8” marketing claim, or a
finite 768-component output proves none of those things by itself.

## Current support map

“Integrated” means cfetch contains the provider/session path. “Certified”
means a real device/runtime passed exact conformance and the required placement
evidence was reviewed. Untested devices remain consumers and use remote
inference; they are never silently called producers.

| Hardware class | cfetch path | INT8 common-denominator evidence | Producer status |
|---|---|---|---|
| x86-64 CPU / AVX | FastEmbed + ORT CPU | canonical S8S8 Q/DQ graph; no float learned-weight copy | **Certified reference** on the tested x86-64 host with Microsoft's official ORT 1.28.0 release, 11/11 exact |
| arm64 CPU | FastEmbed + ORT CPU | same graph; official Microsoft Linux arm64 and macOS arm64 runtime archives pinned | Integrated, physical KAT pending |
| Apple Metal / ANE | ORT Core ML EP | Core ML supports 8-bit model optimization; actual device/compute-plan placement is hardware-generation dependent | Integrated, physical Metal and ANE certificates pending |
| Intel CPU / GPU / NPU | ORT OpenVINO EP | OpenVINO exposes INT8 execution across supported devices, but partitioning is graph/device specific | Integrated, physical per-device certificates pending |
| AMD XDNA2 NPU | ORT Vitis AI or reviewed Ryzen AI lowering | XDNA2/Ryzen AI exposes INT8; AMD's published UINT4/BFP16 EmbeddingGemma is not the v1 artifact | Integrated ORT path, physical XDNA2 conversion/KAT/placement pending |
| AMD GPU | ORT MIGraphX or ROCm EP | MIGraphX accepts quantized ONNX paths on supported consumer/server GPUs; actual operator coverage varies | Integrated, physical RDNA/CDNA certificates pending |
| NVIDIA GPU | ORT CUDA or TensorRT EP | TensorRT explicit quantization uses signed INT8 Q/DQ; no cfetch recalibration is allowed | Integrated, oldest/current architecture certificates pending |
| Qualcomm HTP NPU | ORT QNN EP | QNN HTP has quantized INT8/UINT8 operators; its native signedness differs for some operators | Integrated, physical Snapdragon certificate and exact lowering pending |
| Windows GPU / older mixed vendors | ORT DirectML EP | INT8 support and operator placement depend on driver and adapter | Integrated, physical certificate pending |
| WebGPU / Vulkan-class fallback | ORT WebGPU EP where available | useful coverage for older GPUs is possible, but native W8A8 placement is not assumed | Experimental integration, no producer certificate |

There is no single native 8-bit container that every vendor accelerates.
S8S8, U8U8, layout, fusion and kernel coverage differ. The common denominator
is therefore the frozen logical W8A8 graph plus the exact output KAT—not a
permission to quantize differently per vendor. A native converter may change
container representation only when it preserves all released bytes.

## Known-good CPU floor

The admitted x86-64 package uses Microsoft's official
`onnxruntime-linux-x64-1.28.0.tgz` archive:

- archive SHA-256:
  `a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407`;
- loaded `libonnxruntime.so` SHA-256:
  `1461ef7cc3d9e49982591721683cc3e3a55580aeca9a5254e7aac47b75ee4bab`;
- ONNX Runtime build commit reported by that binary: `da9b5e364c`;
- result: all 11 sequence/language/content KAT records passed through the
  actual Rust → FastEmbed → ORT → cfetch codec path.

The ort-sys bundled/static 1.28 build was deliberately rejected after failing
all 11 records. ORT version strings are therefore informational; distribution
and archive digests are part of producer admission. Linux arm64 and macOS
arm64 packages pin Microsoft's official archives too, but that is packaging
evidence only until those binaries execute the KAT on their target machines.

## Running a certificate

Use the actual package and extracted, separately licensed model bundle:

```console
cfetch inference-certify \
  --model-dir ./cfetch-embeddinggemma-300m-a8w8-v1 \
  --provider auto \
  --json > cfetch-inference-certificate.json
```

`auto` is a package property, not runtime roulette. A provider package contains
one selected accelerator route; the generic local package selects CPU. Explicit
provider names are available for test packages. Accelerator sessions use
static shapes and fail initialization if ORT would fall back to CPU.

The report records the profile/artifact digests, OS/architecture, cfetch,
FastEmbed and ORT identities, pinned runtime distribution/archive, provider,
fallback policy, graph settings, tokenizer tensor digests, raw model-output
digests, exact encoded vectors and pass/fail results. It contains no hostname,
username, private path, credentials, environment dump or private model input.

For CPU, exact KAT plus an admitted runtime is sufficient because the
canonical graph audit establishes W8A8 coverage. For every accelerator, exact
KAT is necessary but not sufficient: attach a vendor profiler/compute-plan
export proving full graph ownership and INT8 learned-region kernels. A human
review then adds a passing record to the certification registry. The release
catalog may reference only reviewed passing records with the exact same model
and runtime/container digests.

## Physical test matrix

The minimum useful tester pool is deliberately explicit:

| Family | Required physical coverage |
|---|---|
| CPU | x86-64 generic/older AVX, x86-64 v3/v4, Linux arm64, Apple arm64 |
| Apple | oldest and newest supported Apple silicon; Metal and ANE placement separately |
| Intel | OpenVINO CPU, integrated/discrete GPU, first and current NPU generations |
| AMD NPU | XDNA2 device through the real Ryzen AI/Vitis runtime |
| AMD GPU | an older supported RDNA consumer GPU and a current RDNA/CDNA device; Linux MIGraphX/ROCm and Windows path separately |
| NVIDIA | oldest supported and current CUDA architecture; CUDA and TensorRT separately |
| Qualcomm | Snapdragon X-class Windows arm64 device using QNN HTP |
| Older/mixed GPU | each advertised DirectML, WebGPU or Vulkan-class package on its minimum adapter |

A certificate is reusable only for the exact artifact, derived container,
runtime, provider, driver/device family and package build that it names.
Upgrading a driver or runtime does not automatically revoke a result, but the
new combination remains non-producing until it passes again.

## Public CI and outside testers

Public-repository standard GitHub-hosted runners are used so the jobs do not
consume the owner's Actions budget. `.github/workflows/accelerator-discovery.yml`
reports only devices exposed to the hosted VM. Its first Apple run found CPU
and an Apple paravirtual GPU but no Neural Engine; an arm64 runner label is not
ANE evidence.

The public certification workflow accepts only an HTTPS model-bundle URL plus
its required SHA-256, builds the real package, verifies/extracts the archive,
runs the KAT, and uploads the JSON report. No secret or private runner is
required. Hosted runners can certify only hardware actually exposed to them;
physical testers use the same command and attach the profiler evidence.

Primary implementation references:

- [ONNX Runtime quantization](https://onnxruntime.ai/docs/performance/model-optimizations/quantization.html)
- [ONNX Runtime execution providers](https://onnxruntime.ai/docs/execution-providers/)
- [QNN execution provider](https://onnxruntime.ai/docs/execution-providers/QNN-ExecutionProvider.html)
- [TensorRT explicit quantization](https://docs.nvidia.com/deeplearning/tensorrt/latest/inference-library/work-quantized-types.html)
- [Core ML optimization overview](https://apple.github.io/coremltools/docs-guides/source/opt-overview.html)
- [OpenVINO execution provider](https://onnxruntime.ai/docs/execution-providers/OpenVINO-ExecutionProvider.html)
- [Vitis AI execution provider](https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html)
- [MIGraphX execution provider](https://onnxruntime.ai/docs/execution-providers/MIGraphX-ExecutionProvider.html)
- [DirectML execution provider](https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html)
