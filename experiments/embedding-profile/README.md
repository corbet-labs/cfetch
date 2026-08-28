# Global embedding compatibility admission

cross_backend_eval.py is the vector-compatibility gate for the shared INT8
space. Package admission additionally requires independently verified
placement, sequence coverage, latency, memory, and provenance evidence.
No device or runtime supplies the numerical reference. For `n` submitted
backend scopes, the evaluator runs all `n x n` ordered
query-backend/document-backend pairings, including every self-pair. Every
pairing must independently meet the same fixed absolute SciFact floors:

- NDCG@10: 0.767907905520953
- Recall@100: 0.970
- MRR@10: 0.7305529100529101

It also tests the derive-once store's real mixed-producer case. For each query
backend, relevant documents take their minimum score and irrelevant documents
their maximum score across every document backend. This adversarial matrix is
stricter than any real store, where each document's first-writer producer is
fixed.

Each backend runner writes an .npz file containing:

- metadata: one JSON string with schema_version, profile_id,
  profile_manifest_sha256, admission_policy_sha256, model, model_revision,
  vector_encoding,
  supported_max_tokens, supported_sequence_buckets,
  supported_max_batch_size, sequence_semantic_fixture_id,
  sequence_semantic_fixture_sha256,
  sequence_capability_evidence, sequence_capability_evidence_sha256, dataset,
  dataset_revision, scope_id, backend, runtime,
  compiler, package_target, artifact_source, artifact_sha256,
  attestation_public_key, internal_precision, device, device_class, placement_evidence,
  placement_evidence_sha256, performance_evidence,
  performance_evidence_sha256, and accelerated_placement: true;
- queries and documents: the canonical signed INT8x768 vectors in pinned
  SciFact order;
- queries_repeat and documents_repeat: a second run on the same
  runtime/artifact/device;
- sequence_probe_queries, sequence_probe_relevant_documents, and
  sequence_probe_irrelevant_documents: one pinned semantic triple for every
  sequence bucket, in `32..2048` bucket order;
- the same three sequence-probe arrays with `_repeat` suffix: a second actual
  adapter run at every graph shape;
- sequence_capability_evidence_bytes, placement_evidence_bytes, and
  performance_evidence_bytes: the exact raw evidence files whose SHA-256
  values appear in metadata;
- wire_batch_outputs: the retained canonical output for every grouping size 1
  through 64, stored as exact signed INT8 with shape `(64, 64, 768)`.

Every vector-bearing array, including the rank-three wire-batch output array,
must be stored with the exact signed 8-bit integer dtype; the evaluator rejects
values merely convertible to INT8, rejects `-128`, and requires every row to
contain `-127` or `+127` as the canonical max-absolute codec does. The repeat
arrays must
be byte-identical to their first run in the same backend/runtime/artifact/device
scope. Arrays from different scopes do not have to be byte-identical.
Cross-backend equality and cosine may be computed out of band for debugging,
but are not serialized in the authoritative report and never determine
admission. The sequence-shape semantic fixture is the narrow exception: for
every query-producer x document-producer pairing and every bucket, its relevant
document must rank strictly before its irrelevant document under production's
exact INT8 comparator. For each query producer and bucket, the gate also takes
the relevant minimum and irrelevant maximum across all document producers and
requires that adversarial mixed-scope ordering to pass.

## Export one adapter scope

`export_adapter_cache.py` is the backend-neutral bridge between a target-native
local adapter and the global retrieval gate. It loads only the revision-pinned
SciFact rows, applies the frozen query and document prompts before sending any
text, and calls an attested OpenAI-shaped loopback `/embeddings` endpoint in
batches. The endpoint must return one explicitly indexed, finite, non-zero
768-component float vector per input. The exporter applies the canonical
max-absolute, round-ties-even signed INT8 codec, runs the complete query and
document scope twice, and refuses non-repeatable output.

For example:

    python experiments/embedding-profile/export_adapter_cache.py \
      --endpoint http://127.0.0.1:8080/embeddings \
      --scope-id apple-coreml-ane-example \
      --backend apple-coreml-ane \
      --runtime "Core ML 8.0" \
      --compiler "coremltools <version>" \
      --package-target aarch64-apple-darwin \
      --artifact-source "<repo>@<revision>/<file>" \
      --artifact-sha256 "$ARTIFACT_SHA256" \
      --attestation-public-key "$ATTESTATION_PUBLIC_KEY" \
      --internal-precision "int8 weights, fp16 activations/output" \
      --supported-max-tokens 2048 \
      --supported-sequence-bucket 32 \
      --supported-sequence-bucket 64 \
      --supported-sequence-bucket 128 \
      --supported-sequence-bucket 256 \
      --supported-sequence-bucket 512 \
      --supported-sequence-bucket 1024 \
      --supported-sequence-bucket 2048 \
      --sequence-capability-evidence results/apple-coreml-sequences.json \
      --sequence-capability-evidence-sha256 "$SEQUENCE_EVIDENCE_SHA256" \
      --device "Apple Neural Engine" \
      --device-class npu \
      --placement-evidence results/apple-coreml-placement.json \
      --placement-evidence-sha256 "$PLACEMENT_EVIDENCE_SHA256" \
      --performance-evidence results/apple-coreml-performance.json \
      --performance-evidence-sha256 "$PERFORMANCE_EVIDENCE_SHA256" \
      --accelerated-placement \
      --batch-size 32 \
      --output results/apple-coreml-ane.npz

`--accelerated-placement` is an explicit evidence assertion, not automatic
device detection. Supply it only when the native adapter's placement report
proves the named accelerator. An adapter requiring loopback authentication can
read its bearer token through `--bearer-token-env NAME`; the token is not
written to the cache. The admission exporter requires the proposed package's
64-character lowercase hexadecimal Ed25519 public key. It sends a fresh nonce,
verifies the signature over the exact request and response bytes, and records
the public key in cache metadata. Unsigned candidate smoke probes are not
admission caches. The exporter refuses non-loopback hosts, redirects, implicit
output paths, and overwriting an existing cache.

All three evidence files are UTF-8 JSON, are embedded byte-for-byte in the
cache, and must identify the same scope ID, backend, artifact source/digest,
runtime, compiler, package target, internal precision, device, and device
class as the CLI. Sequence evidence contains one result per declared bucket
with equal requested/tokenized/executed lengths, 768-dimensional finite
non-zero output, and `truncated: false`. It also covers every batch size 1
through 64 over the same ordered inputs—the first 32 pinned queries followed by
the first 32 pinned documents—with the exact request count and input/output
digests for each grouping. The exporter reruns all 64 groupings and requires
identical canonical output bytes. It persists every grouping's complete
canonical output in `wire_batch_outputs`; registry replay recomputes each
digest from those bytes, checks the pinned ordered-input digest, and proves all
64 retained outputs are byte-identical. Placement evidence
confirms accelerator execution for every bucket, records all fallback work
explicitly, rejects unexpected fallback, and binds profiler output digests.
Performance evidence records sample count, p50/p95 latency, peak memory, and
measured energy—or an explicit reason it was not measurable—separately for
every bucket. Each placement bucket includes `profiler_output_sha256`; each
performance bucket includes `benchmark_output_sha256`.

Every bucket also runs the profile-pinned
`cfetch-sequence-semantic-v1-cat-vs-music` query/relevant/irrelevant triple
twice through the actual adapter. Response token counts must select that exact
bucket, both canonical runs must be byte-identical, and their input/output
digests must match the sequence evidence. The exporter requires the relevant
document to beat the irrelevant document in the self-scope; the global gate
requires the same strict ordering for every ordered query/document scope pair
and for the derive-once relevant-minimum/irrelevant-maximum combination, whose
two records may come from different document scopes.

CLI evidence paths are host-local and are not published. The three validated
JSON summaries are embedded byte-for-byte in the cache under stable `npz:...`
logical locators. Raw profiler and benchmark outputs are retained separately
in the measurement bundle described below.

The exporter never invents sequence capability from the profile. The package
must explicitly record its supported maximum and every supported bucket plus
the evidence location. The global gate loader requires the complete profile set
`32, 64, 128, 256, 512, 1024, 2048`; a fixed-seq256 artifact can produce a
diagnostic cache but cannot pass the profile gate by succeeding on short
SciFact text alone.

Run the compatibility gate with every already-admitted scope, every new scope
named for this admission attempt, and at least one cache from every device
class. Cache labels must equal their embedded scope IDs:

    python experiments/embedding-profile/cross_backend_eval.py \
      --candidate-scope intel-npu-candidate \
      --candidate-scope nvidia-gpu-candidate \
      --candidate-scope xnnpack-cpu-candidate \
      --backend intel-npu-candidate=results/intel-npu.npz \
      --backend nvidia-gpu-candidate=results/nvidia-gpu.npz \
      --backend xnnpack-cpu-candidate=results/xnnpack-cpu.npz \
      --output results/cross-backend-report.json

The evaluator automatically adds every scope already present in
`release/inference-backends.json` to the required set and refuses missing,
unexpected, or identity-mismatched admitted scopes. `passed` means vector
compatibility for that complete set; it is not by itself a package-admission
or hardware-placement verdict. Hash the report's exact bytes and commit it at
exactly `release/admission/<sha256>.json`; a cohort name, mutable `current`
path, symlink, or filename that disagrees with the bytes is rejected. Every
admission updates every admitted registry entry—old and new—to that newest
full-cohort report path and digest.

Reports form a bounded, content-addressed lineage inside the repository. The
genesis report has both `parent_report` and `parent_report_sha256` set to JSON
`null`, has no `already_admitted_scopes`, and names the complete nonempty
initial cohort as `candidate_scopes`. A successor names its parent's exact
`release/admission/<sha256>.json` path and digest. Its
`already_admitted_scopes` must equal the parent's complete cohort exactly, and
its nonempty `candidate_scopes` must be exactly the new current scopes absent
from that parent. Retained backend metadata and cache digests cannot change
across an edge. CI walks newest-to-genesis with depth and total-byte bounds;
missing, rewritten, cyclic, split, shrinking, or internally inconsistent
history fails closed.

That update is an atomic release boundary. Every later cohort requires every
previously published target package to be rebuilt and republished so its
signed production response carries the new global report digest. The new
packages and the registry that binds all entries to that one path/digest pair
must roll out together. A package that still attests an earlier cohort report
is stale and must no longer be accepted as current; mixed old/new package
generations are not a valid admitted cohort.

## Durable admission replay

An admitted scope has two immutable cfetch release assets:

- `<admission_cache_sha256>.npz` is the exact exporter cache;
- `<measurement_evidence_sha256>.zip` retains every raw profiler and benchmark
  output referenced by the embedded summaries.

Both registry URLs must be credential-free
`https://github.com/corbet-labs/cfetch/releases/download/<tag>/...` locators.
The measurement ZIP contains only `measurement-manifest.json` and
`raw/<sha256>.bin` members. Its manifest has schema version 1, the scope ID,
the placement and performance summary digests, and one `{path, sha256, roles}`
entry per raw file. Roles are `placement-profiler` and/or
`performance-benchmark`. Every referenced per-bucket digest must be present;
unreferenced files and duplicate JSON keys are rejected.

CI runs:

    python experiments/embedding-profile/cross_backend_eval.py \
      --verify-release-registry

With no admitted scopes this is a network-free successful no-op. Otherwise it
downloads every exact cache and measurement bundle with per-file and cohort
byte bounds and a 64-scope ceiling, verifies their SHA-256 values, rejects
unsafe ZIP/NPY members before NumPy allocation, validates all three embedded
JSON schemas and registry bindings, and walks every committed report from the
newest digest-named file to genesis. Each lineage node is rebuilt from its
exact cache subset: the full `n x n`, adversarial mixed-store, per-bucket,
persisted wire-batch, and combined gate decisions are replayed. Integer
results, identities, and verdicts compare exactly. Only the three recomputed
floating quality values permit a `1e-12` absolute replay tolerance for
platform-library last-bit drift. Optional cross-backend byte/cosine
observations remain local debugging data outside the report and admission.

This makes report arithmetic and retained bytes independently replayable. It
does not turn CI into silicon attestation: whether the raw profiler output was
captured honestly on the named physical device, or whether a vendor tool's
summary correctly describes that device, remains the reviewed runner,
profiler, and measurement-provenance trust boundary. Generic CI validates the
summary schema, bindings, retained raw-output digests, and report arithmetic;
it cannot vendor-neutrally derive placement, latency, memory, or energy claims
from arbitrary profiler formats.

The exporter cannot include `compatibility_report_sha256` in its challenged
responses because that report is created only after all cohort caches exist.
Candidate nonces and signatures are generated and verified live by the
exporter, but the nonce, raw request/response bodies, and signatures are not
persisted in the cache and therefore are not replayed by registry CI. The
cache persists the verified public key and exact semantic outputs, not a claim
that a past challenge can authenticate a future package.
After admission, packaging injects the final report digest into the production
adapter response/manifest without changing the evaluated artifact, runtime,
compiler, package target, public key, or scope identity. The packaged adapter
then reruns response-schema, nonce-signature, and scope-conformance tests before
release. This repository does not pretend that post-admission package wiring
already exists.

The admission implementation is byte-bound separately from the semantic and
policy identities. Its domain-separated digest covers the exact bytes and
paths of `cross_backend_eval.py`, `admission_evidence.py`,
`export_adapter_cache.py`, `scifact_contract.py`, and
`requirements-test.txt`. That digest is pinned outside the bundle, recorded in
the registry, and embedded in every report. Run:

    python experiments/embedding-profile/cross_backend_eval.py \
      --verify-implementation-bundle

The command recomputes the framed bundle hash and checks both pins. This makes
the exporter/evaluator implementation used for admission an exact-byte
identity rather than an unverified script name or mutable environment. It is
not the target adapter package identity: the cache separately binds that
scope's native artifact digest and detailed runtime/compiler/device identity,
and final package conformance remains required.

The committed report contains only an allowlisted projection of public
registry-bound metadata, never local evidence paths or unknown backend extras.
Production adapters must attest the final compatibility-report digest.

The evaluator uses the exact versions in `requirements-test.txt`. The shared
dataset loader checks the revision-pinned `mteb/scifact` contract has 300 test
queries, 339 positive qrels, and 5,183 ordered documents before export or
evaluation.
Ranking uses the same exact signed-INT8 dot products, norm cross
multiplication, sign ordering, and insertion-order tie break as production
Rust; floating-point BLAS scores are not part of admission.
Model execution and hardware placement remain the responsibility of each local
target-native adapter using the artifacts listed in
release/inference-backends.json; the exporter deliberately contains no vendor
runtime code.

## Activation blockers

Two P1 release boundaries must be implemented and exercised before the first
nonempty admitted cohort:

- Hermetic nonempty admission orchestration must construct a complete cohort
  from pinned target inputs, produce its caches and raw measurement bundles,
  write the digest-named report lineage, replay the nonempty registry, and
  stage the registry/reports/packages as one reproducible transaction without
  hidden manual state. It must also lock and verify the complete Python
  dependency resolution, including transitives and artifact hashes; the
  current requirements file pins only the direct policy-test dependencies.
- Final package conformance must run the actual packaged implementation bytes
  after the newest report digest is injected. The same release transaction
  must verify the report-bound admission implementation-bundle digest,
  requested scope and response schema, a fresh nonce signature,
  artifact/runtime/device bindings, and the newest global report digest. It
  must then replay every admitted sequence-bucket fixture and the same 64
  ordered inputs under every wire grouping size from 1 through 64; all
  canonical INT8 outputs must byte-match the fingerprints retained in that
  scope's admission cache before publication.

`admitted_backends` and `local_packages` remain empty. No target scope or local
producer is admitted until both boundaries and a real complete cohort pass.
