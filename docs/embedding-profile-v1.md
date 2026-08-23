# cfetch embedding profile v1

cfetch has one embedding and network compatibility profile per major. A major
is a data-format boundary, not a marketing label: changing the model,
checkpoint, tokenizer, quantization, prompts, pooling, normalization,
dimensions, or vector codec changes the meaning of every stored vector.

Such a change requires a new cfetch network major and a coordinated
re-embedding. Peers from different majors do not connect, exchange slices, or
rank each other's vectors. This compatibility major is independent of the
pre-1.0 Cargo package version.

Run `cfetch embedding-profile` (or `--json`) to read the executable contract.

## Frozen v1 contract

| Field | v1 value |
|---|---|
| Network/profile | `1` / `cfetch-embedding-v1` |
| Model source | `google/embeddinggemma-300m-qat-q8_0-unquantized` |
| Source revision | `7b5b24595322ab0ea4d08827066860a6df8cb0aa` |
| Model execution | XINT8: signed symmetric INT8 W8A8 with power-of-two scales |
| Canonical artifact | `cfetch-embeddinggemma-300m-xint8-v1` |
| Tokenizer | tokenizer at the same pinned source revision |
| Context | at most 2,048 tokens |
| Sequence shapes | fixed next-power-of-two buckets: 32, 64, 128, 256, 512, 1,024, 2,048 |
| Inference batch | exactly one input per model execution |
| ORT CPU intra-op threads | exactly one |
| ORT execution mode | sequential |
| Query input | `task: search result \| query: {content}` |
| Document input | `title: none \| text: {content}` |
| Pooling | attention-mask-weighted mean, prompt included |
| Runtime graph optimization | ONNX Runtime Level 3 (FastEmbed default) |
| Output normalization | L2, then canonical INT8 vector encoding |
| Dimensions | full 768; no Matryoshka truncation |
| Interchange vector | exactly 768 signed INT8 components / 768 bytes |

There are two distinct uses of INT8 here. The model contract is XINT8 W8A8:
weights and activations are signed symmetric INT8 and use power-of-two scales.
The interchange contract is the final `INT8×768` vector. The pinned Google
repository is a Q8_0-QAT *source* checkpoint with unquantized tensors; it is
not itself the deployed XINT8 artifact. The exact quantized tensors, scales,
operator set, calibration manifest, and canonical ONNX Q/DQ artifact are part
of the v1 release artifact and may not be regenerated independently.

An accelerator may use its native container (OpenVINO IR, Core ML package,
QNN context, TensorRT engine, and so on), but it must be derived from that
canonical artifact and produce the same canonical output bytes. Container
conversion is packaging, not permission to choose another model or quantizer.
In particular, AMD's published Ryzen AI 1.8 EmbeddingGemma package is not the
v1 model: AMD documents it as asymmetric UINT4 weights with BFP16 activations.
cfetch requires its own XINT8 export and certification for XDNA2.

## Canonical vector codec

For one finite, non-zero 768-component model output:

1. L2-normalize the output.
2. Find the largest absolute component `m`.
3. Encode each component as
   `round_ties_even(clamp(component / m * 127, -127, 127))`.
4. Store the signed values in component order as their 768 raw bytes.

The per-vector factor is not serialized. Cosine similarity is invariant under
that positive factor, so an FP16 scale trailer would add a second format
without adding ranking information. cfetch compares INT8 cosine order by
integer dot products and squared norms, using `u128` cross multiplication.
Square roots and floating-point comparison therefore cannot reorder ties on
different CPUs.

The shared store is the record. A producer writes a content-addressed vector
once; every other participant fetches those exact bytes and never re-embeds
that content. If a second producer ever supplies the same content hash under
the same profile, cfetch compares the canonical record byte-for-byte and
rejects a mismatch as cross-runner drift.

Accelerator releases must additionally pass the profile's checked-in
known-answer conformance set before they are allowed to publish vectors.
That set is generated once with the canonical v1 artifact and released with
the first certified runner; no accelerator package is a producer before both
exist. Merely loading a graph, returning finite numbers, or using a vendor's
feature named “INT8” is not sufficient. Until a backend has that proof, it may
consume shared vectors and request remote inference, but it is not a v1
producer.

Batch composition is not an input to the vector. After tokenization and
truncation, each text is padded to the smallest v1 sequence bucket that holds
it and executed individually. Batch-longest padding is forbidden because ORT integer fusion was observed to change
canonical bytes when an unrelated longer text changed the tensor shape.
Network-major-1 producers also execute one input at a time. This deliberately
trades bulk throughput for a batch dimension that cannot vary across hosts;
future majors may change that transaction only with new vectors and a full
re-embedding. ORT uses sequential execution and CPU reductions use one
intra-op thread so the host's core count cannot select another accumulation
order.

The endpoint path enforces the same admission boundary. A successful OpenAI-
shaped response must also return `cfetch_profile`, `cfetch_model_revision`,
`cfetch_profile_manifest_sha256`, `cfetch_model_quantization`, and
`cfetch_model_artifact` fields equal to the executable manifest, plus the
requested `model`. An unattested generic endpoint cannot publish v1 data.

## Compatibility enforcement

The profile identity is recorded in both the local cache metadata and the
shared vector-store header. Old configurable stores use different filenames
and formats and are not treated as v1 data. Configuration that requests a
different model, width, precision, or prompt is rejected with an error naming
the required major-version/re-embedding operation.

Remote TCP requests carry `network_major: 1`. iroh uses a major-specific ALPN,
and invites, grants, and remembered memberships carry the same major. Missing
or different values fail closed before data is served. Package patch/minor
versions within network major 1 may interoperate only while they implement
this exact profile.

Reranking is deliberately outside this boundary. It is computed per query,
is never stored or exchanged as vector truth, and may use another model or be
disabled without changing the embedding/network major.

## Creating v2 or later

A profile change is one coordinated transaction:

1. Freeze the complete successor manifest and its conformance vectors.
2. Assign the next network major; never mutate an existing profile ID.
3. Build and certify every producer backend against the new byte outputs.
4. Stop cross-major networking and create a separate shared-store namespace.
5. Re-embed every content hash once and distribute the new records.
6. Upgrade all participants, then enable the new network together.
7. Retain or remove the old store explicitly; never reinterpret it in place.

Store size determines how long step 5 takes. That cost is why no field in a
published major is “just configuration.”
