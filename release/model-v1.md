# cfetch Embedding Profile v1 model artifact

This release contains the separately licensed model bundle for cfetch network
major 1. It is not a cfetch software release.

- Artifact: `cfetch-embeddinggemma-300m-a8w8-v1`
- Bundle SHA-256:
  `12892e4fb2dea4e60adc03669f32dcee2813d2764c8bf6c25ecf6b95aa5756b1`
- `model.onnx` SHA-256:
  `ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1`
- Source: `google/embeddinggemma-300m-qat-q8_0-unquantized`
- Source revision: `7b5b24595322ab0ea4d08827066860a6df8cb0aa`
- Deployment: static signed-symmetric W8A8 INT8, S8S8 Q/DQ, ONNX opset 18
- Output profile: full 768 dimensions and cfetch signed `INT8x768`

The archive contains the model, exact tokenizer, build and retrieval reports,
artifact lock, source model card, current Gemma terms and prohibited-use
policy, modification notice, and per-file checksums. It was rebuilt
independently and produced the same archive bytes.

The model files are Gemma Model Derivatives and are not licensed under the
cfetch software license. Downloading, using, modifying or redistributing them
is subject to the `GEMMA_TERMS.html` and prohibited-use policy included in the
archive. No training, fine-tuning, distillation or pruning was performed.

The first admitted runner is the Nix x86-64 CPU package using Microsoft's
exact official ONNX Runtime 1.28.0 shared release; the real Rust/FastEmbed/ORT
path passes all 11 known-answer vectors byte for byte. Other CPU, GPU and NPU
paths remain consumer-only until their physical certification reports pass.
