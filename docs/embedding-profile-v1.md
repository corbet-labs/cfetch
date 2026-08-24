# cfetch embedding profile v1

cfetch has exactly one embedding space per network major. Network major 1 is
the immutable profile below: one model, one tokenizer and prompt pipeline, one
quantized graph, and one 768-byte vector record. CPU, GPU and NPU are runners
for that profile. They are not allowed to choose their own model or
quantization.

A change to the checkpoint, tokenizer, prompts, context or shape policy,
pooling, graph quantization, dimensions, normalization, or vector codec is a
breaking data change. It requires network major 2, 3, and so on, a separate
store namespace, and re-embedding every record. Different majors never
connect, exchange slices, or rank each other's vectors. This network major is
independent of cfetch's pre-1.0 software version.

`cfetch embedding-profile --json` prints the executable contract.

## Frozen contract

| Field | Network-major-1 value |
|---|---|
| Profile | `cfetch-embedding-v1` |
| Model source | `google/embeddinggemma-300m-qat-q8_0-unquantized` |
| Source revision | `7b5b24595322ab0ea4d08827066860a6df8cb0aa` |
| Canonical graph | `cfetch-embeddinggemma-300m-a8w8-v1` |
| Graph SHA-256 | `ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1` |
| Model quantization | static signed-symmetric W8A8 INT8, S8S8 Q/DQ, ONNX opset 18 |
| Weights | INT8 per output channel |
| Activations | INT8 per tensor with frozen calibration scales |
| Accumulators | INT32 |
| Dimensions | full 768; no Matryoshka truncation |
| Context | at most 2,048 tokens |
| Shapes | batch 1; sequence buckets 32, 64, 128, 256, 512, 1,024, 2,048 |
| Query input | `task: search result \| query: {content}` |
| Document input | `title: none \| text: {content}` |
| Model output | graph-owned pooling, projection and L2-normalized `sentence_embedding` |
| ORT execution | sequential, one CPU intra-op thread, `ORT_ENABLE_ALL` graph optimization |
| Interchange vector | signed `INT8x768`, exactly 768 bytes |

The source repository is Google's full 300M EmbeddingGemma Q8-QAT source. Its
tensors are unquantized; it is not the deployed runtime. The separately
licensed cfetch artifact is a deterministic static W8A8 export of that source.
No training, fine-tuning, distillation, pruning, or task-specific model change
is part of the build.

The model bundle is deterministic and separately licensed under the included
Gemma terms. Its current archive SHA-256 is
`12892e4fb2dea4e60adc03669f32dcee2813d2764c8bf6c25ecf6b95aa5756b1`.
The immutable archive is published in the public
[`model-v1` release](https://github.com/corbet-labs/cfetch/releases/tag/model-v1).
The archive includes the graph, tokenizer, source card, build report,
retrieval audit, artifact lock, terms, modification notice, and per-file
checksums. The graph digest above, rather than the archive filename or URL, is
the network identity.

## What “one INT8 model” means

All learned embedding and MatMul inputs in the canonical graph are covered by
INT8 Q/DQ nodes, and the artifact contains no floating-point shadow copy of
the learned weights. W8A8 execution still requires INT32 accumulation and
floating-point-domain residual, nonlinear, normalization, and final-output
operations. Those are arithmetic inside this single frozen graph, not an
FP16/FP32 model alternative or fallback.

S8S8 Q/DQ is the canonical ONNX representation because it is the documented
ONNX Runtime CPU/GPU default and matches TensorRT's signed explicit-INT8 path.
It is not a claim that every vendor consumes the same integer container.
Qualcomm HTP, for example, exposes important native operators as unsigned
quantized pairs. A vendor converter may lower the frozen semantics into a
native container, but it may not recalibrate, change scales, requantize the
model, or substitute a different precision. The only proof that a lowering is
compatible is the released byte-level known-answer test.

This avoids the false choice between one file format and one vector space:
v1 freezes one numerical graph and one final record. Provider-specific
containers are admitted only when they reproduce that record exactly.

## Deterministic execution boundary

FastEmbed owns the tokenizer and common model API. ONNX Runtime owns graph
execution and vendor execution-provider integration. cfetch selects the
graph's named `sentence_embedding` output directly; it does not let a library
guess a token output, pool it again, or normalize it again.

Each input is prefixed, tokenized, truncated, padded to its smallest allowed
bucket, and executed alone. Batch-longest padding is forbidden: testing found
that changing an unrelated neighbor's shape can alter integer graph fusion
and therefore the final byte. CPU reductions are sequential and
single-threaded. Accelerator sessions use fixed batch and sequence dimensions.

Runtime provenance is certification evidence. An ort-sys static build and
Microsoft's official shared build both reported ONNX Runtime 1.28.0, but the
static build failed all 11 v1 vector answers while Microsoft's release passed
all 11. The Nix local-CPU package therefore pins Microsoft's exact archive
bytes and records their digest. “Same ORT version” is not sufficient.
The Rust/FastEmbed binding uses the older ORT C API 18 surface so compatible
vendor runtimes are not excluded merely by a newer compile-time ABI request;
the loaded runtime's exact distribution and bytes still have to be certified.

The same official ORT 1.28 build commit was then exercised on public Linux
arm64 and macOS arm64 runners. Both loaded and ran successfully but matched
none of the 11 x86 reference records. Hundreds of components changed in every
answer, and the two arm64 OS runtimes themselves diverged on five answers.
They are rejected producers, not “probably compatible” CPU packages. This is
why shared vectors are derived once and distributed, and why an architecture
name or matching runtime version cannot bypass the KAT.

A physical AMD Radeon RX 6800 produced the complementary accelerator result.
ORT's MIGraphX provider could not own the whole graph with CPU fallback
disabled. Standalone MIGraphX did compile and run the complete frozen W8A8
graph on RDNA2 INT8-capable kernels, but its first encoded result changed 729
of 768 components. This proves that “same ONNX Q/DQ graph” and “real INT8
acceleration” still do not imply identical output arithmetic across kernels.
It also rules out bypassing ORT with direct MIGraphX as a v1 producer.

These results settle the apparent common-denominator problem at the correct
boundary. V1 has one logical W8A8 model and one canonical signed `INT8x768`
record, but it cannot honestly promise that every vendor kernel derives those
bytes independently. A device is a producer only if its exact host/runtime
passes the byte KAT; every other device consumes the same already-derived
record or uses a certified remote producer. That consumer path is fully
interoperable across CPU, GPU and NPU because inference precision is never
part of the shared record. There is no approximate compatibility mode.

Run the real packaged path with:

```console
cfetch inference-certify --model-dir ./cfetch-embeddinggemma-300m-a8w8-v1 --provider auto --json
```

The command verifies every bundle file before loading ONNX, runs public inputs
covering every sequence bucket and multiple languages/content types, applies
the canonical codec, and compares all 11 records byte for byte. Accelerator
packages additionally disable ORT CPU fallback. A passing accelerator vector
test still needs reviewed placement/profiler evidence that learned W8A8
regions actually used the claimed INT8 device kernels.

The ordinary local embedding path enforces the same gate before it can answer
or write a vector. A bundle or session that merely loads is not admitted. On a
host whose runtime produces different bytes, local initialization fails with a
consumer-only message. Existing shared records and a separately configured
certified remote producer remain the compatible routes. `inference-certify`
deliberately remains available on such a host so it can emit the failure report
needed to add or reject that runtime.

## Canonical vector codec

The graph's named output is already L2-normalized. For one finite, non-zero
768-component graph output, the vector codec:

1. Finds the largest absolute component `m`.
2. Encodes each component as
   `round_ties_even(clamp(component / m * 127, -127, 127))`.
3. Stores the signed values in component order as their 768 raw bytes.

The positive per-vector factor is not serialized because cosine similarity is
invariant under it. cfetch compares INT8 cosine order using integer dot
products, squared norms and `u128` cross multiplication, so floating-point
square roots cannot reorder ties on different hosts.

The shared store is the record. A producer derives a content-addressed vector
once; peers fetch those exact bytes rather than re-embedding the content. If a
second producer supplies a different record for the same content and profile,
cfetch rejects it as drift. Store headers, endpoint attestations, iroh ALPN,
invites, grants and memberships all carry the major/profile boundary and fail
closed on a mismatch.

Uncertified hardware remains fully useful as a consumer: it can search shared
vectors or request inference from a certified producer. It must not advertise
local producer capability. The release catalog remains remote-only until a
standalone local release variant assembles the published model, provider
runtime, and reviewed certificate. The source flake's `cfetch-local-cpu`
package and public model bundle can already be used directly for evaluation.

Reranking is deliberately outside this boundary. It is transient per query,
never stored or exchanged as vector truth, and may use a different model or be
disabled without changing the network major.

## Creating the next major

A successor profile is one coordinated transaction:

1. Freeze its complete manifest, model artifact and known-answer bytes.
2. Assign the next major; never mutate `cfetch-embedding-v1`.
3. Build and certify producer backends against the new answers.
4. Prevent cross-major networking and create a new store namespace.
5. Re-embed each content hash once and distribute the new records.
6. Upgrade every participant before enabling the new network.
7. Retain or remove the old store explicitly; never reinterpret it in place.

The re-embedding cost grows with store size. That is why every major is
intentionally breaking and why no v1 field is user-configurable.
