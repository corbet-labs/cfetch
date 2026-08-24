# Embedding v1 artifact build

This directory records how the separately licensed cfetch network-major-1
model artifact was built, evaluated, locked, and packaged. Nothing here trains,
fine-tunes, distils, or prunes a model. The release artifact is identified by
digest and is not part of the cfetch software binary.

## Frozen result

| Field | Value |
|---|---|
| Artifact | `cfetch-embeddinggemma-300m-a8w8-v1` |
| Model SHA-256 | `ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1` |
| Source | `google/embeddinggemma-300m-qat-q8_0-unquantized` |
| Source revision | `7b5b24595322ab0ea4d08827066860a6df8cb0aa` |
| Learned regions | static signed-symmetric W8A8 Q/DQ; per-channel weights, per-tensor activations |
| Calibration | Entropy, 16 deterministic samples, 256 tokens, SmoothQuant `0.65` |
| Graph | ONNX opset 18; 171 learned nodes covered by Q/DQ |
| Output | full 768 dimensions, then the canonical signed `INT8x768` codec |

`v1-artifact-lock.json` is the human- and machine-readable release decision.
It pins the model, build report, retrieval audit, source export, and known-answer
digests. The bundler verifies that lock before producing an archive.

The source checkpoint contains floating-point tensors even though Google
trained it as a Q8-QAT source. The exported full-precision ONNX graph is used
only as an offline quantizer input and evaluation reference. It is never a
cfetch runtime, fallback, package, release artifact, or vector format.

## Reproducible build

The canonical source export was made with the pinned toolchain below:

```bash
MODEL_ROOT=/solid/agents/mind/models/cfetch/embedding-v1

uv run --python 3.12 \
  --with 'torch==2.8.0+cpu' \
  --with 'sentence-transformers==5.2.0' \
  --with 'transformers==4.57.1' \
  --with 'optimum-onnx[onnxruntime]==0.1.0' \
  --index 'https://download.pytorch.org/whl/cpu' \
  --index 'https://pypi.org/simple' \
  optimum-cli export onnx \
  --model "$MODEL_ROOT/source/7b5b24595322ab0ea4d08827066860a6df8cb0aa" \
  --library-name sentence_transformers \
  --task sentence-similarity \
  --dtype fp32 \
  --batch_size 2 \
  --sequence_length 16 \
  "$MODEL_ROOT/work/export-fp32"
```

The frozen candidate was then produced with AMD Quark `0.12.post1`, ONNX
`1.22.0`, ONNX Runtime `1.25.1`, Transformers `4.57.1`, NumPy `2.5.2`, and
PyTorch `2.8.0+cpu`:

```bash
uv run --python 3.12 \
  --with 'torch==2.8.0+cpu' \
  --with 'amd-quark==0.12.post1' \
  --with 'onnxruntime==1.25.1' \
  --with 'transformers==4.57.1' \
  --index 'https://download.pytorch.org/whl/cpu' \
  --index 'https://pypi.org/simple' \
  experiments/embedding-v1/quantized_artifact.py \
  --model "$MODEL_ROOT/work/export-fp32/model.onnx" \
  --tokenizer "$MODEL_ROOT/source/7b5b24595322ab0ea4d08827066860a6df8cb0aa" \
  --quantization A8W8 \
  --calibration entropy \
  --per-channel-weights \
  --smooth-alpha 0.65 \
  --node-selection weighted-matmul \
  --samples 16 \
  --max-tokens 256 \
  --no-hardware-optimize \
  --output candidate.onnx
```

The deterministic calibration corpus covers queries, prose, Markdown, source
code, shell output, structured data, and multiple languages. An independent
second build produced the exact same ONNX and build-report bytes.

## Evaluation, not training

`runtime_kat.py` derives the eleven public known-answer records. It executes
one input at a time, pads to the profile's fixed buckets, and applies the final
signed `INT8x768` codec. ORT `1.25.1` and `1.28.0` produced the same eleven
vector digests on CPU.

`retrieval_eval.py` is a diagnostic retrieval benchmark on the complete
SciFact test split at revision
`cf10ab6856b15b0e670ef8ae5dae4e266c12d035`. SciFact is never used for
training or calibration. The chosen artifact measured:

| Metric | Source export | W8A8 artifact | Delta |
|---|---:|---:|---:|
| NDCG@10 | 0.77791 | 0.74665 | -0.03126 |
| Recall@100 | 0.97500 | 0.97167 | -0.00333 |
| MRR@10 | 0.74055 | 0.70440 | -0.03616 |

An earlier exploratory threshold rejected that regression and proposed
quantization-aware distillation. That was the wrong product direction: cfetch
v1 requires the unmodified general-purpose Google model, one 8-bit deployment
artifact, and no training. The report remains immutable evidence, while the
artifact lock records the deliberate product decision to accept the strongest
bounded, conventional, hardware-neutral W8A8 candidate.

The screen also tested MinMax calibration at 16 and 128 samples, SmoothQuant
alphas `0.3` through `0.9`, Percentile, Entropy, and AdaQuant. The Entropy
`0.65` candidate above was best. Adding more MinMax calibration did not recover
quality. QuaRot was not adopted because it inserts additional rotation/
Hadamard transforms whose portable execution and INT8 placement are not
established across the target providers; that conflicts with v1's common-
denominator priority.

## What the quantization statement means

All learned embedding and MatMul inputs are covered by signed INT8 Q/DQ, and
the graph contains no floating-point shadow copy of the learned weights. W8A8
still uses INT32 accumulators and floating-point-domain nonlinear, residual,
normalization, and final-output operations. Those are required arithmetic
inside this single graph, not alternate FP16/FP32 model weights or fallbacks.

The canonical ONNX encoding is S8S8 because that is ORT CPU/GPU's documented
default and NVIDIA TensorRT's supported explicit-quantization form. It is not
claimed to be a universal native vendor container. Every provider must run the
same frozen semantics, disable CPU fallback, and match all final `INT8x768`
known answers before it can publish vectors. A provider may compile a native
cache, but it may not recalibrate, change scales, or choose another numerical
scheme.
