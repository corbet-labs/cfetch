# Apple Core ML candidate smoke

This experiment answers one narrow question: can the pinned public Core ML
EmbeddingGemma package execute two cfetch-prefixed inputs on an Apple Silicon
runner and produce valid, immediately repeatable canonical signed INT8x768
records?

It is deliberately a candidate smoke test, not backend admission. The public
artifact is fixed to 256 tokens, while the complete cfetch profile supports up
to 2048 tokens. Its runtime silently truncates longer inputs to that compiled
shape, so cfetch must not expose it as a complete profile implementation.
Requesting `.cpuAndNeuralEngine` is also not proof that every operation ran on
the Apple Neural Engine. Admission still requires the missing fixed-length
buckets, placement evidence, and the global ordered all-pairs plus adversarial
mixed-store retrieval gates described in the parent directory.

The experiment pins both inputs:

- `CoreML-LLM` source revision
  `5ef6b301d3a3d628e25c0605479f59dbf3a7d955`;
- `valindotai/embeddinggemma-300m-coreml` artifact revision
  `d1dc305086782e958f91fa278de97e4af9caeaf0`, with SHA-256 checks for every
  downloaded runtime file.

The artifact card declares `google/embeddinggemma-300m` as its base model, and
its bundled tokenizer/config files are byte-identical to those at cfetch's
target revision. It does not identify the exact weights revision or conversion
commit used to build the compiled encoder, however. The smoke therefore
reports encoder lineage to cfetch's required source revision as unproven. The
matching model family and tokenizer are useful candidate evidence, not profile
attestation.

The Swift program passes already-prefixed query and document strings with
`task: nil`, validates finite, non-zero 768-dimensional output, applies
cfetch's max-absolute round-to-nearest-even codec, checks the model's L2-unit
normalization, rejects identical query/document records, and requires the
canonical bytes from an immediate second inference to match. It prints one
bounded JSON record containing hashes and diagnostics, never the full vectors.
The package also commits the complete Swift dependency lock used by the smoke.

Run the GitHub Actions workflow **Apple Core ML candidate** on its standard
`macos-26` Apple Silicon runner. The workflow downloads rather than commits the
large Core ML asset. The artifact remains subject to its upstream Gemma terms;
this experiment does not redistribute it.
