# cfetch Embedding Profile v1 model artifact

This release contains the separately licensed model bundle for cfetch network
major 1. It is not a cfetch software release.

- Artifact: `cfetch-embeddinggemma-300m-a8w8-v1`
- Bundle SHA-256:
  `be377d9d3a4ff53e092898e30369dd64d368dc0ff803fbe62d7538c391d9d20f`
- `model.onnx` SHA-256:
  `ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1`
- Source: `google/embeddinggemma-300m-qat-q8_0-unquantized`
- Source revision: `7b5b24595322ab0ea4d08827066860a6df8cb0aa`
- Deployment: static signed-symmetric W8A8 INT8, S8S8 Q/DQ, ONNX opset 18
- Runtime contract: deterministic compute and precise/non-saturating QMM
- Output profile: full 768 dimensions and cfetch signed `INT8x768`

The archive contains the model, exact tokenizer, build, retrieval and runtime
KAT reports, artifact lock, source model card, current Gemma terms and
prohibited-use policy, modification notice, and per-file checksums. It was
rebuilt independently and produced the same archive bytes.

The model files are Gemma Model Derivatives and are not licensed under the
cfetch software license. Downloading, using, modifying or redistributing them
is subject to the `GEMMA_TERMS.html` and prohibited-use policy included in the
archive. No training, fine-tuning, distillation or pruning was performed.

The reference runner is the Nix x86-64 CPU package using Microsoft's exact
official ONNX Runtime 1.28.0 shared release. With the frozen precise-QMM
policy, the real Rust/FastEmbed/ORT path produces the same 11 raw outputs and
known-answer vectors on physical AMD AVX2 and Intel VNNI CPUs and on one
recorded hosted AMD EPYC 7763. This is deliberately host-scoped: current
hosted Arm routes failed exact conformance, and a hosted Intel Xeon 8573C
control exposed a sequence-size-dependent mismatch despite extensive INT8 ISA
support. Every actual producer host must pass the startup KAT. Other CPU, GPU
and NPU paths remain consumer-only until their certification reports pass.
