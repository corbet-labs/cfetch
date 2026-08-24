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
| x86-64 CPU / AVX | FastEmbed + ORT CPU | canonical S8S8 Q/DQ graph; deterministic compute; precise QMM selects non-saturating U8U8 on pre-VNNI x86 | **Passing reference** with Microsoft's official ORT 1.28.0: physical Ryzen 9 5950X AVX2 and Core Ultra 7 258V VNNI hosts produced the same 11 raw outputs and final records; every actual host remains KAT-gated |
| arm64 CPU | FastEmbed + ORT CPU | same graph; official Microsoft Linux arm64 and macOS arm64 runtime archives pinned | Previous runs used the superseded candidate KAT; **current schema-2 recertification pending**, so arm64 remains consumer-only |
| Apple Metal / ANE | ORT Core ML EP | Core ML supports 8-bit model optimization; actual device/compute-plan placement is hardware-generation dependent | **Rejected on the hosted virtual Apple probe**: both GPU and NPU routes placed all 2,212 logged operations on Core ML's CPU and left other nodes on ORT CPU; a physical-device certificate and alternative runtime/graph route remain pending |
| Intel CPU / GPU / NPU | ORT OpenVINO EP | OpenVINO exposes INT8 execution across supported devices, but partitioning and numerical results are graph/device specific | **CPU passes through ORT CPU precise QMM; OpenVINO GPU/NPU rejected on physical Lunar Lake** because strict ownership failed. The former hybrid vector comparison is superseded; alternative full-graph/exact accelerator paths remain pending |
| AMD XDNA / XDNA2 NPU | ORT Vitis AI | Ryzen AI 1.8 exposes an A8W8 compiler and broad A8W8 operator coverage, but its compatibility table promises INT8 only for CNNs; the newer Windows ML route requires A16W8 for quantized Transformers | Rust session path and a local package builder for AMD's installed deployment runtime are integrated; physical X1/X2 KAT and operator-assignment reports remain pending, and no AMD NPU is yet a producer |
| AMD GPU | ORT MIGraphX EP | MIGraphX compiled the complete frozen graph to 512 GPU code objects on a physical RDNA2 RX 6800, including 168 quantized dot and one quantized GEMM occurrence | Reproducible Nix/ORT/MIGraphX package integrated, but **rejected**: ORT left nodes on forbidden CPU fallback; standalone full-GPU MIGraphX emitted digest `2639d019…`, not corrected-v1 `20e16438…`; ORT's older ROCm EP was removed in 1.23 and is not a second current path |
| NVIDIA GPU | ORT CUDA or TensorRT EP | TensorRT explicit quantization uses signed INT8 Q/DQ; no cfetch recalibration is allowed | Official ORT CUDA 12 runtime and separate Nix-pinned CUDA/TensorRT evidence packages integrated; package tests pass, but oldest/current physical architecture KAT and placement certificates remain pending |
| Qualcomm HTP NPU | ORT QNN EP | QNN HTP has quantized INT8/UINT8 operators; its native signedness differs for some operators | Microsoft QNN 1.24.4 Windows ARM64 evidence package integrated; a hosted Cobalt ARM VM had no HTP and rejected backend initialization, so physical Snapdragon HTP KAT/placement remains pending |
| Windows GPU / older mixed vendors | ORT DirectML EP | INT8 support and operator placement depend on driver and adapter | Microsoft DirectML 1.24.4 x64/ARM64 evidence package integrated; the hosted Hyper-V display exposed no matching DirectML device, so each physical adapter still needs exact KAT and placement evidence |
| WebGPU / Vulkan, D3D12, Metal fallback | ORT native WebGPU plugin EP | one official cross-vendor plugin targets Linux/Vulkan, Windows D3D12/Vulkan and macOS/Metal, but this Q/DQ graph needs unsupported `QuantizeLinear` | **Rejected on physical RDNA2**: strict ownership failed; hybrid tracing showed dequantization plus `f32` MatMul shaders and stable incompatible bytes. Hosted Windows/virtual macOS also left CPU nodes; this is not a W8A8 producer path today |

There is no single native 8-bit container that every vendor accelerates.
S8S8, U8U8, layout, fusion and kernel coverage differ. The common denominator
is therefore the frozen logical W8A8 graph plus the exact output KAT—not a
permission to quantize differently per vendor. A native converter may change
container representation only when it preserves all released bytes.

## Known-good CPU floor

The x86-64 reference package uses Microsoft's official
`onnxruntime-linux-x64-1.28.0.tgz` archive:

- archive SHA-256:
  `a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407`;
- loaded `libonnxruntime.so` SHA-256:
  `1461ef7cc3d9e49982591721683cc3e3a55580aeca9a5254e7aac47b75ee4bab`;
- ONNX Runtime build commit reported by that binary: `da9b5e364c`;
- deterministic compute: enabled;
- precise QMM: enabled;
- result: all 11 sequence/language/content KAT records pass through the
  actual Rust → FastEmbed → ORT → cfetch codec path.

The precise-QMM field is load-bearing. ORT documents that U8S8 quantized
matmul on AVX2 and AVX-512 without VNNI may use `VPMADDUBSW`; each pair of
products is clamped to a signed 16-bit intermediate and can saturate. Its
`session.x64quantprecision` option selects slower U8U8 arithmetic to avoid
that overflow. The first pre-release KAT accidentally canonized the saturating
path on a Ryzen 9 5950X.

A controlled four-way test isolated the cause:

| Path | Result |
|---|---|
| Ryzen AVX2, former defaults | reproducible former candidate bytes |
| Ryzen AVX2, deterministic compute only | identical former candidate bytes |
| Ryzen AVX2, deterministic compute + precise QMM | corrected 11/11 reference |
| Intel Core Ultra 7 258V with VNNI | byte-identical to corrected Ryzen, including every raw float output |

The rebuilt Nix package's physical Ryzen report SHA-256 is
`cddf55f783ae5d7a256f1d09be3087f6d2c25a49d4adef2c0e6245180b9c07b5`;
its hardware capture SHA-256 is
`c83ec99a6cf7d6c39c6287ca11d1ebf66fc8e82e519159c4439dcf71d459054e`.
The physical Intel report SHA-256 is
`af48bdb87317241155e5e8cba18a206ca80e19d59fd338af75a6063c5ddbe538`;
its hardware capture SHA-256 is
`c437e642a844f919f182869ab2e8d686dd87b57634b4998127d9c0d0e2944c49`.

The graph itself did not change. The runtime policy, KAT, executable profile
digest and bundle metadata did. This reset occurred before any tagged cfetch
release could produce network-major-1 vectors and while the catalog remained
remote-only. The former profile and all reports against it remain in the
registry as superseded pre-release evidence. After a v1-capable release or
producer catalog ships, the same numerical change requires network major 2.

Former hosted x86 and arm64 reports must therefore not be read as current
passes or failures. Linux arm64 and macOS arm64 remain consumer-only until the
official packages are rerun against report schema 2. Ordinary production
always executes the current KAT on the host actually doing work.

cfetch and its FastEmbed fork normally request ORT C API 18, the lowest API
used by the built-in provider/session code, while the CPU reference runtime
remains Microsoft's 1.28.0 release. The C API is backward-compatible, so this
lowers only the minimum loadable vendor-runtime ABI; it does not change the
graph, optimizer, or vector bytes. It covers current 1.23-class MIGraphX and
QNN packages. The WebGPU-only package requests API 22 because plugin device
discovery does not exist below that surface.

## AMD XDNA evidence package and precision boundary

AMD's current Ryzen AI 1.8 deployment documentation explicitly lists the
INT8 application DLL set, supports on-device compilation, and exposes an
operator-assignment report. `scripts/build-windows-vitis-package.ps1` builds
cfetch against those exact bytes from an already installed Ryzen AI tree,
hashes every copied source file, and emits a self-checking evidence package.
It does not download, publish, or license AMD's proprietary runtime. XDNA2
Strix/Krackan uses the current `X2` target without an xclbin. First-generation
Phoenix/Hawk Point uses `X1` plus AMD's `phoenix/4x4.xclbin`. The cfetch
provider keeps optimization level zero, creates a different compiler cache
key for every frozen sequence bucket, and disables ORT CPU fallback.

The silicon's INT8 throughput does not establish graph compatibility. AMD's
Ryzen AI 1.8 table advertises CNN INT8 and NLP BF16, even though its newer
integer compiler and operator table contain A8W8 support. Its separate Windows
ML VitisAI table is stricter: a quantized QDQ CNN may be A8W8, but a quantized
Transformer must be A16W8. AMD's own optimized EmbeddingGemma 1.8 artifact is
UINT4-weight/BFP16-activation, not cfetch's frozen W8A8 model. Neither route is
an interchangeable v1 substitute. The only unresolved route is to submit the
unchanged cfetch graph to the installed classic VitisAI compiler on physical
X1/X2 hardware; session construction must own the full graph, all 11 vectors
must match, and every bucket's assignment report must prove NPU placement.

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
reproduces the failure boundary.

The flake also exposes `cfetch-test-openvino-current`, built entirely from the
locked nixpkgs revision: ONNX Runtime 1.27.1 plus OpenVINO 2026.3.0, with
fixed ORT source SHA-256
`8b6bbf2677db27fb2bb196370136f662c0415c48531a16adb2bdfef5e1d55773`
and loaded-library SHA-256
`4679de3b061093b85262b0a79466f05d31068a90750c740cd3c592350471b4de`.
It is an explicit evidence lane for current Intel drivers, not a catalogue
producer. Host driver/compiler libraries remain outside the Nix closure, so a
matching NixOS driver stack is the reproducible deployment target; on other
Linux distributions the system compiler's loader/glibc ABI must be bridged
and recorded explicitly.

That current lane was exercised on a physical Core Ultra 7 258V with Arc 140V
GPU and Lunar Lake NPU. Native OpenVINO 2026.3 compiled the unchanged frozen
1x32 graph for both `GPU.0` and `NPU`, proving that neither device nor graph
compilation was the immediate blocker. Strict cfetch sessions nevertheless
failed because the ORT provider requested forbidden CPU remainder. A
controlled diagnostic then enabled that remainder explicitly to test the only
remaining shortcut; it exercised all 11 inputs and seven buckets but produced
vectors incompatible with the superseded candidate KAT. Those numerical
comparisons are not current certificates. The strict ownership failure remains
current because the graph did not change, and production remains no-fallback.
Each Intel accelerator still needs an alternative route that both owns the
full graph and passes the corrected bytes.

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
`20e164382888d264f9a8db999c8f375740c18f0df384ca4335a2d1b75e2971b1`.
The old 729-component comparison is retained only as superseded evidence; the
distinct current digest is enough to reject exact conformance. Thus RDNA2 does
accelerate this graph, but a direct MIGraphX backend is not a compatible v1
producer and is not a useful escape from ORT's partitioning failure.

The MIGraphX package is intentionally an explicit certification target rather
than a default flake check: compiling the ROCm closure on every public CPU-only
CI run would consume substantial time while proving no GPU execution. It must
be built and exercised on the actual AMD device under test.

## NVIDIA CUDA and TensorRT evidence packages

Linux x86-64 exposes `cfetch-test-cuda` and `cfetch-test-tensorrt`. Both start
from Microsoft's official
`onnxruntime-linux-x64-gpu_cuda12-1.28.0.tgz`, archive SHA-256
`ea6bd2b65d7dfabbeb92c4af5dd8f12e5aed8601e544ad378d2f872275438b1a`.
That archive contains the CUDA and TensorRT provider libraries. The CUDA
package includes only CUDA EP plus Nix-pinned CUDA 12.9 `cudart`, cuBLAS and
cuRAND libraries; the physical host supplies its NVIDIA driver and
`libcuda.so.1`. Its loaded ORT library SHA-256 is
`87097979b341c4df9c1bf71b14f7376f84a91206fbc64c0ccc4733dcbbab9e40`
and CUDA provider SHA-256 is
`958b1b20df4177c10418bfc203898aab85bda9504afaba108a88775dd0aa0539`.
The complete cfetch package test suite passes without an NVIDIA device.

TensorRT is separate because its provider also requires cuDNN and TensorRT
itself. Their runtime-only upstream wheels are hash-pinned Nix inputs. They
retain NVIDIA's non-redistributable licenses, stay in the tester's local Nix
store, and are never copied into a cfetch release artifact. The runtime-only
route avoids nixpkgs' 8.55 GiB complete TensorRT SDK archive while preserving
the libraries ORT actually links. The TensorRT 10.16.1.11 wheel SHA-256 is
`8e45036efeb964d323231544442a73619201136ccc84392560254cc8f0d516e4`;
the cuDNN 9.22.0.52 wheel SHA-256 is
`391b9a7ee6386daaca7f8dca41e83c2c99f760c9581a0400755e87b4287b8847`.
The assembled ORT TensorRT provider SHA-256 is
`4e4fd8e65341ff80698d9051c7cd3badcee07d050fa03b95c7124b311849fa0c`.
The complete cfetch package test suite passes. Building it is only provenance
and ABI evidence: neither route becomes a producer until a physical NVIDIA
GPU passes all KAT bytes with fallback disabled and supplies reviewed
device/INT8 placement evidence.

On the available AMD-only host, the CUDA certificate predictably stopped
before session construction because `libcuda.so.1` was absent. That confirms
the package did not fall back to CPU; it is not an NVIDIA hardware result and
does not appear in the accepted or rejected device registry.

## Windows DirectML, Qualcomm QNN and Intel OpenVINO evidence packages

`scripts/build-windows-inference-package.ps1` creates a self-checking local
package for `directml`, `qnn`, `openvino-cpu`, `openvino-gpu`, or
`openvino-npu`; the companion certification script verifies
every packaged file and the frozen model bundle before running cfetch. DirectML
uses Microsoft's
`Microsoft.ML.OnnxRuntime.DirectML` 1.24.4 NuGet, archive SHA-256
`57e9f11b73437bef7a309496135d4c1f96b1a8e9ddba60013fa27bfc1d788681`,
and supports the package's x64 or ARM64 native runtime. QNN uses
`Microsoft.ML.OnnxRuntime.QNN` 1.24.4, archive SHA-256
`e4d6eabb9e503d4f3c78494fc9400f02509b2ee315d9f707644a174ece8da17f`,
and intentionally refuses anything except native Windows ARM64. The QNN
package carries Microsoft's ORT QNN provider and Qualcomm's CPU/GPU/HTP
runtime libraries and notices; cfetch selects `QnnHtp.dll` and still forbids
ORT CPU fallback.

The OpenVINO variants all pin Intel's
`Intel.ML.OnnxRuntime.OpenVino` 1.24.1 NuGet, archive SHA-256
`f53ad5f90e3d616970a5c65e4880ebbe92c9774e9727020661db591cea74a110`.
Its Windows x64 native directory contains one ORT/OpenVINO build plus the
OpenVINO 2025.4.1 CPU, GPU and NPU plugins. The provider name selects the
device; it does not change the graph or package bytes. The package therefore
refuses ARM64 rather than substituting a different runtime.

The public workflow uses `windows-latest` for DirectML and
OpenVINO CPU and `windows-11-arm` for QNN. These are packaging and
adverse-execution probes on the hardware actually exposed to each hosted
runner. A Windows ARM runner name is not proof of a Snapdragon HTP, a virtual
display adapter is not proof of a physical DirectML GPU, and hosted OpenVINO
CPU execution is not Intel GPU/NPU evidence. The uploaded hardware JSONL
deliberately contains only OS/build, CPU name, GPU name/vendor/driver, and
compute-accelerator friendly name/status—never PnP IDs, PCI addresses, UUIDs
or serials.

Public run
[`32693600516`](https://github.com/corbet-labs/cfetch/actions/runs/32693600516)
exercised all three packages. The Cobalt 100 ARM VM exposed no Qualcomm HTP;
QNN returned `QNN_BACKEND_ERROR_CANNOT_INITIALIZE` and cfetch rejected the
remaining CPU assignment. The x64 VM exposed only Microsoft Hyper-V Video, so
DirectML found no matching device. Windows OpenVINO reached session
construction on the hosted AMD CPU but did not own the complete graph. No KAT
vector was emitted in any case, and none of these hosted results is relabeled
as Qualcomm, DirectML-GPU or Intel-hardware evidence.

## Cross-vendor native WebGPU evidence package

ONNX Runtime's native WebGPU EP is now an official plugin rather than an
experimental provider compiled into the core runtime. Version 0.2.1 requires
ORT 1.24.4 or newer and uses Dawn to reach Vulkan on Linux, D3D12 or Vulkan on
Windows, and Metal on macOS. This is the best maintained single route for
older mixed-vendor GPUs, but cross-vendor API coverage is not cross-vendor
numerical identity.

`cfetch-test-webgpu` combines Microsoft's official ORT 1.28 core with
`Microsoft.ML.OnnxRuntime.EP.WebGpu` 0.2.1. The plugin NuGet SHA-256 is
`a707557c86eb1eee0a604146ac4edc473d5af0bfe2fc77fd632217755cbfb282`;
the extracted Linux x86-64 plugin library SHA-256 is
`45e1c7465ada0d85f63565358b4c8350d8b0f523948a0e2c7d0d20697f8918e9`,
and the macOS universal library SHA-256 is
`8fac874a60f32f0127c74cb7def915807fcc8a6c30b77629e45f8cee60272eae`.
The Nix package includes the Vulkan loader on Linux but never substitutes a
software renderer or a driver. Microsoft's current plugin has no Linux ARM64
binary, so that platform is explicitly absent instead of being mislabeled.

The Windows builder pairs the plugin with
`Microsoft.ML.OnnxRuntime` 1.28.0, archive SHA-256
`769d1d3ea8ab6cd69f737c9dd4d4462aa4ad0ccfa106eaf506efc40d7bead5db`.
The x64 and ARM64 plugin library SHA-256 values are respectively
`be2ebcc0a96d1558d9123c04e75c2851260fe45c9dbc8959cb2cd8d11b83abbe`
and
`63cfef0e7fb8fdc2238f69cd8e804f50fda393b2b60c448daec73e031de75058`.
Both the core archive and plugin archive/library identities are recorded in
the package manifest and certificate.

This explicit hardware-probe package compiles the complete WebGPU feature but
does not rerun cfetch's unrelated application integration tests. Those remain
blocking platform checks. Two identical macOS attempts had otherwise stopped
in an iroh test-server destructor before loading Metal; the probe's next step
is the exact inference certificate, so no provider failure is being hidden.

Plugin EPs use ORT's device API, so only this evidence package raises the Rust
binding surface from C API 18 to API 22. The core runtime remains 1.28 and the
frozen graph, FastEmbed pipeline, optimizer, static buckets, fallback rule,
and vector codec do not change. FastEmbed selects one concrete registered
WebGPU device for each session: ORT rejects a single session configured with
devices originating from separate adapter factories even when they expose the
same EP name. A local AMD host without an installed Vulkan
ICD loaded the exact plugin and then failed with `No supported adapters`; that
is a useful fail-closed package probe, not a GPU result.

The corrected one-device public probe in run
[`32693600516`](https://github.com/corbet-labs/cfetch/actions/runs/32693600516)
reached the real provider boundary on Windows: the plugin registered a
concrete device, but the frozen graph left nodes on forbidden ORT CPU fallback
when the VM exposed only Microsoft Hyper-V Video. Linux loaded the same plugin
and rejected `No supported adapters` because the runner had no Vulkan driver.
The isolated macOS probe in run
[`32695710770`](https://github.com/corbet-labs/cfetch/actions/runs/32695710770)
loaded the pinned universal plugin on a recorded virtual Apple M1 and reached
session initialization. It too left graph nodes on forbidden ORT CPU fallback,
so cfetch rejected it before emitting a KAT vector. These results prove package
loading and fail-closed selection only; they do not certify a physical Vulkan,
D3D12 or Metal adapter.

The same package was then exercised on a physical Radeon RX 6800 through
Vulkan. Strict initialization failed because WebGPU had no kernel for
`QuantizeLinear` and several graph-shaping operators; ORT would have assigned
those nodes to CPU. A controlled hybrid diagnostic was used only to inspect
the remaining GPU work. Its generated MatMul shader declared `f32` inputs,
output and accumulation after `DequantizeLinear`; it was not executing the
learned regions as W8A8. Three complete runs produced identical raw-output and
final-vector hashes as the reported GPU temperature changed from roughly
39–41°C to 41–43°C. The mismatch is deterministic operator lowering, not heat,
and native WebGPU is rejected for this v1 graph today.

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

Report schema 2 records the profile/artifact digests, OS/architecture, cfetch,
FastEmbed and ORT identities, pinned runtime distribution/archive, provider,
fallback policy, deterministic-compute and precise-QMM settings, graph
settings, tokenizer tensor digests, raw model-output digests, exact encoded
vectors and pass/fail results. It contains no hostname,
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
| AMD NPU | Phoenix/Hawk Point XDNA (`X1`) and Strix/Krackan XDNA2 (`X2`) through the real Ryzen AI/Vitis runtime |
| AMD GPU | an older supported RDNA consumer GPU and a current RDNA/CDNA device; current Linux MIGraphX, native WebGPU and Windows paths separately |
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

On Linux x86-64, `nix build .#cfetch-test-openvino` builds the official-wheel
Intel evidence package, while `nix build .#cfetch-test-openvino-current`
builds the locked ORT 1.27.1/OpenVINO 2026.3 lane for current host drivers.
Both are non-catalogue tools. The public runner exercises `openvino-cpu`;
physical systems select `openvino-gpu` or `openvino-npu`. The packages disable
dynamic shapes and use one stream/thread, while the seven frozen cfetch
buckets remain unchanged. The first local CPU attempt was rejected at
full-graph ownership. Public run
[`32681875052`](https://github.com/corbet-labs/cfetch/actions/runs/32681875052)
reproduced that boundary on a recorded AMD EPYC 7763 hosted VM. That validates
the package and rejection path, not Intel CPU/GPU/NPU support.
Run
[`32693600516`](https://github.com/corbet-labs/cfetch/actions/runs/32693600516)
reproduced the same ownership rejection with both Linux and Windows evidence
packages; its Windows host was also AMD. The later physical Lunar Lake run is
the first Intel GPU/NPU evidence: both current devices compiled the graph but
were rejected by strict ownership. Their deliberately non-certifying hybrid
diagnostic predates the corrected KAT and must not be quoted as a current
byte-count result. First-generation Intel NPU, discrete GPU, and alternative
runtime/graph paths remain open.

On physical Linux AMD systems, `nix build .#cfetch-test-migraphx` builds the
locked ORT/MIGraphX/ROCm evidence package. Its first RX 6800 result is rejected
and registry-pinned as described above. Current RDNA/CDNA devices and a
different compatible runtime path remain open; no AMD GPU producer is
advertised.

On a physical Ryzen AI Windows system, first install the vendor runtime and
driver, then create a local-only evidence package:

```powershell
./scripts/build-windows-vitis-package.ps1 -Output ./cfetch-vitis
./scripts/certify-windows-inference-package.ps1 `
  -Package ./cfetch-vitis `
  -Bundle ./cfetch-embeddinggemma-300m-a8w8-v1.tar.gz `
  -Report ./cfetch-inference-certificate.json `
  -VitisTarget X2
```

For Phoenix/Hawk Point, use `-VitisTarget X1`; the script selects the packaged
`phoenix/4x4.xclbin` unless an explicit path is supplied. Submit the JSON
certificate, `*.vitis-placement` reports and runtime manifest, but never
upload AMD's runtime package or DLLs.

On physical Linux NVIDIA systems, `nix build .#cfetch-test-cuda` builds the
CUDA evidence package. `nix build .#cfetch-test-tensorrt` adds the separately
licensed TensorRT/cuDNN runtime-only inputs. Run the former with `cuda` and the
latter with `tensorrt`; test minimum and current GPU architectures separately.
These large provider closures are explicit targets rather than default
CPU-only flake checks.

On Windows, build and run a provider package from PowerShell 7:

```powershell
./scripts/build-windows-inference-package.ps1 -Provider directml -Output ./cfetch-directml
./scripts/certify-windows-inference-package.ps1 `
  -Package ./cfetch-directml `
  -Bundle ./cfetch-embeddinggemma-300m-a8w8-v1.tar.gz `
  -Report ./cfetch-inference-certificate.json
```

Use `qnn` instead of `directml` only on native Windows ARM64. On Windows x64,
use `openvino-cpu`, `openvino-gpu`, or `openvino-npu` to test Intel's pinned
OpenVINO package. A physical accelerator submission must also attach the
relevant HTP, DirectML or OpenVINO placement/profiling evidence.

On Linux x86-64 or Apple Silicon, build the cross-vendor package with
`nix build .#cfetch-test-webgpu` and certify `webgpu`. On Windows x64 or ARM64,
use `webgpu` with the same PowerShell commands above. Record the actual Dawn
backend and adapter in profiler/debug output. A software Vulkan renderer, a
virtual display adapter, or successful plugin registration without a device
does not count as physical GPU evidence.

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
- [Intel ORT/OpenVINO NuGet](https://www.nuget.org/packages/Intel.ML.OnnxRuntime.OpenVino)
- [Vitis AI execution provider](https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html)
- [Ryzen AI 1.8 model deployment](https://ryzenai.docs.amd.com/en/latest/modelrun.html)
- [Ryzen AI 1.8 application packaging](https://ryzenai.docs.amd.com/en/latest/app_development.html)
- [Ryzen AI 1.8 operator support](https://ryzenai.docs.amd.com/en/latest/ops_support.html)
- [Windows ML VitisAI model support](https://ryzenai.docs.amd.com/projects/WinML/en/latest/model_support.html)
- [MIGraphX execution provider](https://onnxruntime.ai/docs/execution-providers/MIGraphX-ExecutionProvider.html)
- [DirectML execution provider](https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html)
- [Native WebGPU execution provider](https://onnxruntime.ai/docs/execution-providers/WebGPU-ExecutionProvider.html)
- [Microsoft WebGPU plugin package](https://www.nuget.org/packages/Microsoft.ML.OnnxRuntime.EP.WebGpu)
- [ROCm provider removal and MIGraphX migration](https://onnxruntime.ai/docs/execution-providers/ROCm-ExecutionProvider.html)
