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
| x86-64 CPU / AVX | FastEmbed + ORT CPU | canonical S8S8 Q/DQ graph; no float learned-weight copy | **Certified only on named hosts** with Microsoft's official ORT 1.28.0 release, 11/11 exact; a hosted run of the same bytes failed 0/11, so every host remains KAT-gated |
| arm64 CPU | FastEmbed + ORT CPU | same graph; official Microsoft Linux arm64 and macOS arm64 runtime archives pinned | **Rejected** for production with those runtimes: hardware-identified public runs loaded and executed but failed 0/11 exact vectors; alternative runtime pending |
| Apple Metal / ANE | ORT Core ML EP | Core ML supports 8-bit model optimization; actual device/compute-plan placement is hardware-generation dependent | **Rejected on the hosted virtual Apple probe**: both GPU and NPU routes placed all 2,212 logged operations on Core ML's CPU and left other nodes on ORT CPU; a physical-device certificate and alternative runtime/graph route remain pending |
| Intel CPU / GPU / NPU | ORT OpenVINO EP | OpenVINO exposes INT8 execution across supported devices, but partitioning is graph/device specific | Official `cfetch-test-openvino` runtime package integrated for Linux x86-64; local CPU probe rejected because OpenVINO did not own the complete frozen graph; public reproduction and physical GPU/NPU certificates pending |
| AMD XDNA2 NPU | ORT Vitis AI or a reviewed Ryzen AI adapter | XDNA2 exposes INT8, but Ryzen AI 1.8 documents INT8 for CNN and BF16 for NLP, an ORT 1.16 Vitis path, and automatic CPU partitioning | Rust session surface integrated, but no compatible cfetch runtime package yet; an AMD adapter/runtime update and physical KAT/placement proof are required |
| AMD GPU | ORT MIGraphX or ROCm EP | MIGraphX compiled the complete frozen graph to 512 GPU code objects on a physical RDNA2 RX 6800, including 168 quantized dot and one quantized GEMM occurrence | Reproducible Nix/ORT/MIGraphX package integrated, but **rejected**: ORT left nodes on forbidden CPU fallback; standalone full-GPU MIGraphX executed but changed 729/768 bytes on the first KAT |
| NVIDIA GPU | ORT CUDA or TensorRT EP | TensorRT explicit quantization uses signed INT8 Q/DQ; no cfetch recalibration is allowed | Session path integrated; runtime packaging and oldest/current architecture certificates pending |
| Qualcomm HTP NPU | ORT QNN EP | QNN HTP has quantized INT8/UINT8 operators; its native signedness differs for some operators | Session path integrated; Microsoft publishes Windows QNN packages; Snapdragon package/KAT/placement pending |
| Windows GPU / older mixed vendors | ORT DirectML EP | INT8 support and operator placement depend on driver and adapter | Session path integrated; DirectML runtime package and physical certificate pending |
| WebGPU / Vulkan-class fallback | ORT WebGPU EP where available | useful coverage for older GPUs is possible, but native W8A8 placement is not assumed | Experimental session path; runtime package and producer certificate pending |

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

That pass is a reference-host certificate, not an x86-64-wide promise. Public
run [`32680098981`](https://github.com/corbet-labs/cfetch/actions/runs/32680098981)
later loaded the exact same x86 package, archive and library but failed 0/11;
412–739 components changed per answer and cosine remained 0.971–0.983. The
older workflow did not record its CPU model, so the attempt is rejected at run
scope. New reports include a separate non-identifying hardware evidence file.
Ordinary production always executes the KAT on the host actually doing work.
Run
[`32680584417`](https://github.com/corbet-labs/cfetch/actions/runs/32680584417)
subsequently passed 11/11 again with the same package. Because that workflow
also predates hardware capture, this is a second run-scoped passing result.
Run
[`32681875052`](https://github.com/corbet-labs/cfetch/actions/runs/32681875052)
then passed 11/11 on a recorded AMD EPYC 7763 Azure VM while the same official
runtime again failed 0/11 on recorded Arm Neoverse-N2 Linux and virtual Apple
M1 macOS runners. Hardware capture makes each observation reproducible; it
still does not turn a passing host into an architecture-wide package.

The ort-sys bundled/static 1.28 build was deliberately rejected after failing
all 11 records. ORT version strings are therefore informational; distribution
and archive digests are part of producer admission. Linux arm64 and macOS
arm64 packages pin Microsoft's official archives too. Public run
[`32678932373`](https://github.com/corbet-labs/cfetch/actions/runs/32678932373)
executed both on their target architectures and rejected both: 0/11 records
matched. Linux arm64 changed 622–736 components per 768-byte answer and macOS
arm64 changed 410–736; cosine to the x86 records remained about 0.97–0.98.
The two arm64 runtimes agreed exactly on six answers and disagreed on five.
This is architecture/runtime numerical divergence, not a one-bit codec edge.
Neither package may produce v1 vectors; a different runtime must pass from
scratch.

cfetch and its FastEmbed fork request ORT C API 18, the lowest API used by the
provider/session code, while the certified CPU runtime remains Microsoft's
1.28.0 release. The C API is backward-compatible, so this lowers only the
minimum loadable vendor-runtime ABI; it does not change the graph, optimizer,
or vector bytes. It covers current 1.23-class MIGraphX and QNN packages.
AMD's Ryzen AI 1.8 documentation still identifies its Vitis EP as ORT 1.16,
below this Rust binding's provider floor. That row therefore needs an updated
AMD plugin or a small provider-specific adapter before physical testing can
begin; compiling cfetch's `inference-vitis` feature alone is not a runnable
XDNA package.

The Linux x86-64 OpenVINO evidence package extracts only the language-neutral
runtime libraries and notices from Intel's official
`onnxruntime_openvino-1.24.1-cp313-cp313-manylinux_2_28_x86_64.whl`. The wheel
SHA-256 is
`2c3bb73e68ac27f4891af8a595c1faf574ec68b772e6583c90a0b997a1822782`;
its loaded `libonnxruntime.so.1.24.1` SHA-256 is
`a88c790b82c5bdfd4740ebe3018e52009851097f97fbd9e5e3fd3249fcdb9ed7`.
That distribution includes OpenVINO 2025.4.1 CPU, GPU and NPU plugins. Package
provenance is established here. A physical package probe on the available AMD
Ryzen 9 5950X reached OpenVINO session construction but was rejected because
some nodes remained assigned to ORT's default CPU EP. No vector was produced.
OpenVINO's own EP documentation recommends disabling ORT's high-level graph
optimizations for best coverage, but v1 freezes `ort-enable-all` as part of the
shared executable profile. Silently changing that setting for one vendor would
no longer be the same pipeline. An isolated diagnostic build nevertheless
tested `ORT_DISABLE_ALL`; it was rejected at the same full-graph boundary, so
that documented suggestion is not a hidden working route. The public job
reproduces the failure boundary; each selected Intel device still requires a
compatible full-graph path, exact bytes and placement review.

Public runs
[`32680584417`](https://github.com/corbet-labs/cfetch/actions/runs/32680584417)
and
[`32681875052`](https://github.com/corbet-labs/cfetch/actions/runs/32681875052)
also exercised the actual CoreML GPU and NPU routes. Core ML compiled both
MLPrograms, but each plan placed all 2,212 operations on
`MLCPUComputeDevice`: zero selected GPU or Neural Engine. Additional graph
nodes remained assigned to ORT CPU. Because fallback is forbidden, cfetch
stopped at session initialization before emitting any vector. Both evidence
artifacts are registry-pinned. The newer runner identifies itself as
`VirtualMac2,1` / `Apple M1 (Virtual)`, so it is useful rejection evidence but
cannot prove anything about physical Metal or ANE execution.

## Physical AMD RDNA2 result

The pinned flake now exposes `cfetch-test-migraphx` on Linux x86-64. It builds
ONNX Runtime 1.27.1 and MIGraphX/ROCm 7.2.3 from the locked nixpkgs revision.
Nixpkgs' ORT derivation required RTTI to be restored because ORT's MIGraphX
stream bridge uses `dynamic_cast`; every other input remains locked. The
loaded ORT library SHA-256 is
`8b84c85c1e7419c04dd523f7a1313eb9044b24b68c621cd1d00516242b7a0795`
and its MIGraphX provider library SHA-256 is
`20a42d36996d4fb14cca478b306dbaccf48cf8b59f09fa4a63ace57934c40f88`.
The package's full test suite passed before the device probe.

On a physical AMD Radeon RX 6800 (`gfx1030`, Navi 21/RDNA2), the normal
FastEmbed → ORT → MIGraphX route was rejected at session construction because
MIGraphX EP did not own every graph node and ORT CPU fallback was disabled.
This is an ORT partitioner/operator-coverage boundary, not a conclusion that
RDNA2 lacks INT8 execution.

To test that distinction, standalone MIGraphX 2.15.0 parsed, compiled and ran
the complete fixed 1x32 graph on the same GPU without recalibration or another
quantizer. Its compiled program contained 512 `gpu::code_object` occurrences,
168 `mlir_quant_dot` occurrences, one `quant_gemm`, and no `cpu::`
instruction. The exact public first-KAT tensors were used. The resulting
canonical record was
`2639d01960d3a05431eaa6b6b03207f5e0e69ae9b0ce26513fca6472f5d530d9`,
not the required
`7e67364be4c574340d3693b2743c32d0f8841bf9e723bda821d4ab0c85e32984`;
729 of 768 components changed. Thus RDNA2 does accelerate this graph, but a
direct MIGraphX backend is not a compatible v1 producer and is not a useful
escape from ORT's partitioning failure. The registry records all runtime,
hardware, input, output and compiled-program hashes.

The MIGraphX package is intentionally an explicit certification target rather
than a default flake check: compiling the ROCm closure on every public CPU-only
CI run would consume substantial time while proving no GPU execution. It must
be built and exercised on the actual AMD device under test.

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
For source-built Nix provider packages, the schema's legacy
`onnxruntime_archive_sha256` field carries the fixed-output ORT source digest;
the loaded shared library is still hashed independently.

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

On Apple Silicon, `nix build .#cfetch-test-coreml` builds the non-catalogue
CoreML certification package against the same pinned official ORT archive.
The workflow probes `coreml-gpu` and `coreml-npu` separately with MLProgram,
the frozen static sequence buckets, low-precision GPU accumulation disabled,
and Core ML compute-plan logging enabled. The first GPU and NPU probes were
rejected for CPU-only placement and incomplete provider ownership. A hosted
result can prove only the device actually exposed to that runner; it cannot
stand in for another Apple model or an absent accelerator.

On Linux x86-64, `nix build .#cfetch-test-openvino` builds the equivalently
non-catalogue Intel evidence package. The public runner exercises
`openvino-cpu`; physical Intel systems use that same binary with
`openvino-gpu` or `openvino-npu`. The package disables dynamic shapes and uses
one stream/thread, while the seven frozen cfetch buckets remain unchanged. The
first local CPU attempt was rejected at full-graph ownership; the package is a
reproducible evidence tool, not a producer. Public run
[`32681875052`](https://github.com/corbet-labs/cfetch/actions/runs/32681875052)
reproduced that boundary on a recorded AMD EPYC 7763 hosted VM. That validates
the package and rejection path, not Intel CPU/GPU/NPU support.

On physical Linux AMD systems, `nix build .#cfetch-test-migraphx` builds the
locked ORT/MIGraphX/ROCm evidence package. Its first RX 6800 result is rejected
and registry-pinned as described above. Current RDNA/CDNA devices and a
different compatible runtime path remain open; no AMD GPU producer is
advertised.

The public certification workflow accepts only an HTTPS model-bundle URL plus
its required SHA-256, builds the real package, verifies/extracts the archive,
runs the KAT, and uploads the JSON report. No secret or private runner is
required. Hosted runners can certify only hardware actually exposed to them;
physical testers use the same command and attach the report and profiler
evidence through the public [inference hardware run form](https://github.com/corbet-labs/cfetch/issues/new?template=inference_certification.yml).
The form accepts exact passes, byte mismatches, execution failures and setup
blockers. Failed evidence is kept as a rejected attempt; it never becomes a
producer claim. The [public physical-certification tracker](https://github.com/corbet-labs/cfetch/issues/9)
lists the remaining device families and current results.

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
