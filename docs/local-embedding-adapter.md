# Local embedding adapter contract

Status: candidate packaging boundary for `cfetch-embedding-v1`.

cfetch owns the shared semantic profile and the canonical `signed-int8x768`
query/document representation. A local adapter owns only the target-native
runtime and its model artifact. This keeps Core ML, LiteRT, OpenVINO, QNN, and
other vendor SDKs outside the Rust core without turning local inference into a
remote service.

The adapter is a loopback-only process shipped and supervised with its target
package. A non-loopback endpoint remains an explicit remote deployment choice.

## Ownership boundary

cfetch owns:

- the pinned model family and semantic profile revision;
- tokenizer identity, query/document prompts, dimensions, and pooling contract;
- the canonical signed INT8 output codec;
- content-addressed vector storage and distribution;
- ordered query-backend x document-backend and adversarial mixed-store
  retrieval admission.

The adapter owns:

- loading one pinned native artifact;
- selecting and proving its NPU, GPU, or accelerated CPU execution device;
- tokenization and model execution without changing the supplied text;
- returning the pinned scope's certified semantic pipeline as one finite,
  non-zero, 768-dimensional vector per input;
- its artifact, runtime, compiler, device-family, placement, latency, and
  repeatability evidence.

Internal tensor precision is not a network property. An Apple package may use
Core ML FP16 activations, an Android package may use LiteRT mixed INT4/INT8,
and a CPU fallback may use another accelerated representation. They join one
space only after their canonical INT8 outputs pass the same retrieval gate.

## Loopback wire shape

cfetch sends an OpenAI-shaped request to `POST /embeddings`:

```json
{
  "model": "<profile model id>",
  "dimensions": 768,
  "input": ["already-prefixed text"],
  "cfetch_requested_scope_id": "<exact package-plan scope>"
}
```

`cfetch_requested_scope_id` is mandatory on the package-local boundary and is
part of the exact signed request bytes. An explicitly configured remote
endpoint omits it because that deployment owns its admitted scope selection;
remote execution is never inserted after local CPU fallback.

For the production profile the request also carries a fresh 32-byte challenge
as 64 lowercase hexadecimal characters in
`X-Cfetch-Attestation-Nonce`. The adapter returns
`X-Cfetch-Attestation-Signature`, a 64-byte Ed25519 signature encoded as 128
lowercase hexadecimal characters. The signed message is the exact byte
concatenation

    "cfetch-embedding-response-attestation-v1\0"
    || nonce
    || SHA256(exact request body)
    || SHA256(exact response body)

The matching public key is pinned in the admitted registry entry for that
scope and must be globally unique across the admitted cohort. The nonce
prevents replay, and hashing the raw bodies binds the inputs, vectors, row
metadata, and scope attestation without relying on JSON canonicalization. The
admission cache exporter requires the proposed package public key up front
(`--attestation-public-key`), challenges every request with a fresh nonce,
verifies the exact request/response signature before accepting the JSON, and
stores that key in the cache metadata consumed by the cohort report. Earlier
candidate smoke probes may remain unsigned, but they cannot produce an
admission cache. A released producer cannot serve canonical vectors until its
package key and exact cohort report are admitted.

Candidate challenges are live checks, not retained attestations. The exporter
does not persist the fresh nonce, raw request/response bodies, or signature, so
registry CI cannot replay that exchange later. It instead replays the retained
canonical outputs and evidence bindings. Final package conformance must issue
a new challenge against the actual package, verify its response again, and
replay every admitted per-bucket and 1-through-64 wire fixture through those
published implementation bytes. The resulting canonical INT8 records must
byte-match the scope's admitted cache fingerprints; a valid package key cannot
hide adapter or pipeline drift.

The admission exporter necessarily runs before that cohort report exists, so
its challenged candidate responses do not yet carry
`compatibility_report_sha256`. After the report is committed and hashed, the
packaging step injects that final digest without changing the evaluated model
artifact, runtime/compiler settings, package target, scope ID, or attestation
key. Release conformance then reruns the response-schema and nonce-signature
checks and the exact admitted output-fingerprint replay against the final
package. This post-admission wiring is required work, not something the
exporter can truthfully synthesize in advance.

The report itself is committed under its exact digest as
`release/admission/<sha256>.json`. Genesis has no parent and classifies its
entire nonempty cohort as candidates. Each successor points to the previous
digest-named report, classifies that parent's complete cohort as already
admitted, and contains a nonempty set of newly evaluated candidates. All
registry entries always point to the newest global report. Consequently, a
later cohort atomically rebuilds and republishes every prior target package
with the new report digest; packages carrying an older report digest cease to
be current rather than coexisting as another admitted generation.

The admission implementation bundle is a separate exact-byte identity for the
backend-neutral exporter and evaluator, not a name for the target package. Its
domain-separated digest covers the exporter, evaluator, shared evidence
validator, pinned SciFact loader, and exact Python requirements. The digest is
pinned outside those files, recorded in the registry, embedded in every
report, and recomputed with:

    python experiments/embedding-profile/cross_backend_eval.py \
      --verify-implementation-bundle

The target package remains independently bound by its native artifact digest
and exact scope metadata; final conformance must prove that packaging and
report-digest injection did not change those evaluated bindings.

Local evidence filenames and absolute paths are not part of that cache or its
public cohort report. The cache uses stable logical locators for its embedded
JSON evidence byte arrays and binds their exact contents by SHA-256. Admission
also retains every summary-referenced raw profiler and benchmark output in a
content-addressed measurement ZIP; CI verifies those bytes, while reviewed
capture provenance remains the physical-hardware trust boundary. Generic CI
does not vendor-neutrally derive placement, latency, memory, or energy
summaries from arbitrary profiler output; it validates their strict schema,
bindings, raw-output retention, and hashes. Runner identity, profiler meaning,
and honest capture remain reviewed evidence.

For a `supervised-local` scope the signing key is distributed in the package,
so its signature is a request/response consistency check, not an independent
package or silicon identity. A stale or different loopback worker is excluded
by the combined boundary: cfetch re-hashes the exact launcher and root package
manifest, starts that sibling itself, supplies a fresh bearer over stdin, and
owns the child's lifetime. The signature then binds the fresh nonce and exact
wire bytes within that boundary. It cannot defeat an operator who controls and
replaces the installed package or prove accelerator placement by itself.
Placement remains bound to the release's live provider evidence and exact
artifact/runtime/device scope. A `remote-attested` service instead uses an
operator-held, non-distributed key and is a separate transport identity.

cfetch applies the frozen retrieval prompt before sending the request. The
adapter must not add a second task prefix. It returns rows with explicit,
unique input indices and the profile attestation:

```json
{
  "model": "<profile model id>",
  "cfetch_profile": "cfetch-embedding-v1",
  "cfetch_profile_manifest_sha256": "<manifest digest>",
  "cfetch_admission_policy_sha256": "<admission-policy digest>",
  "cfetch_model_revision": "<profile source revision>",
  "cfetch_execution": {
    "scope_id": "<exact admitted scope id>",
    "transport": "supervised-local",
    "backend": "<runtime backend label>",
    "runtime": "<exact runtime and version>",
    "compiler": "<exact compiler/settings identity>",
    "package_target": "<exact target scope>",
    "artifact_source": "<pinned source@revision/file>",
    "device_class": "npu",
    "device": "<exact device family>",
    "artifact_sha256": "<exact native artifact digest>",
    "internal_precision": "<recorded native numeric path>",
    "placement_evidence_sha256": "<placement report digest>",
    "supported_max_tokens": 2048,
    "supported_sequence_buckets": [32, 64, 128, 256, 512, 1024, 2048],
    "supported_max_batch_size": 64,
    "sequence_capability_evidence_sha256": "<sequence report digest>",
    "performance_evidence_sha256": "<per-bucket performance report digest>",
    "compatibility_report_sha256": "<global gate report digest>",
    "accelerated_placement": true
  },
  "data": [
    {
      "index": 0,
      "cfetch_scope_id": "<exact admitted scope id>",
      "token_count": 123,
      "sequence_bucket": 128,
      "truncated": false,
      "embedding": [0.0]
    }
  ]
}
```

The illustrative `embedding` above is shortened. A real row contains exactly
768 finite, non-zero components. cfetch refuses missing attestation, wrong
width, duplicate or missing indices, degenerate vectors, unaccelerated
placement, incomplete sequence coverage, and any execution provenance absent
from this release's admitted registry. Echoing the public profile strings or a
known scope ID cannot self-admit an endpoint because the response must also
verify under the exact package key. Runtime shape/health checks do
not rediscover pooling or normalization; the exact artifact, package, evidence
digests, and compatibility report in the admitted scope are the authority.

`token_count` covers the already-prefixed input including every special token.
The adapter selects the smallest configured sequence bucket that can contain
that count, right-pads behind an attention mask, and rejects inputs above 2,048
tokens. It never truncates. Every row repeats the execution scope ID so a mixed
or stale worker response fails closed. One wire request contains at most 64
inputs.

The sequence evidence binds that batch maximum and covers every wire batch size
from 1 through 64. Every size uses the same ordered inputs: the first 32 pinned
SciFact queries followed by the first 32 pinned SciFact documents, with their
profile prefixes already applied. Each result records 64 inputs,
`ceil(64 / batch_size)` requests, 64 response rows, the SHA-256 of the compact
UTF-8 JSON input array (`ensure_ascii=false`, separators `,` and `:`), and the
SHA-256 of the resulting row-major canonical INT8 bytes. All 64 output digests
must be identical. The admission exporter reruns every grouping and compares
the observed counts and digests to the supplied sequence evidence; endpoint
sizes alone are insufficient. Per-bucket performance evidence records positive
energy joules and average watts when measurable, or explicitly records
`not_measured` with a nonempty reason. Missing measurement prevents an energy
claim but does not by itself fail vector-space compatibility.

Shape coverage is not accepted from shape/finite/non-zero flags alone. For
each bucket the admission exporter sends a profile-pinned cat-vs-music semantic
query/relevant/irrelevant triple twice. It checks the returned token count and
smallest-fitting bucket, exact canonical-byte repeatability, evidence-bound
input/output digests, and strict relevant-before-irrelevant ordering. The cache
stores the six resulting `(7, 768)` INT8 arrays (first and repeat runs). The
global gate then requires that ordering at every bucket for every ordered
query-backend x document-backend pairing. For each query scope and bucket it
also compares the minimum relevant score with the maximum irrelevant score
across document scopes, allowing those two records to come from different
producers just as they can in the derive-once store. A deterministic but
semantically wrong long-shape graph therefore cannot enter the shared space
merely by returning a finite vector or by passing only its homogeneous pair.

## Canonical output codec

Both document and query vectors enter the same codec before ranking or
storage:

1. require the frozen 768-dimensional semantic output;
2. require finite, non-zero model output from the exact certified scope;
3. find the vector's maximum absolute component `m`;
4. encode each component as
   `clamp(round_ties_even(component / m * 127), -127, 127)`;
5. store or score the resulting 768 signed bytes.

No floating-point scale trailer is serialized. Cosine ranking is defined on
the signed integer records, so a vector created on one admitted backend can be
distributed without decoding or requantization. Exact bytes between different
backends are diagnostic; ordered cross-backend retrieval plus the adversarial
mixed-document-store quality test are the gate.

## Packaging and fallback

A target package contains cfetch, its local adapter, the pinned native artifact
or a licensed download recipe, and the adapter evidence manifest. Its single
loopback dispatcher follows the package's embedded admitted-scope list: NPU,
then GPU, then accelerated CPU. Failure to initialize or prove one class
advances locally to the next packaged class. It never silently selects a
remote endpoint.

For each local attempt the dispatcher adds
`cfetch_requested_scope_id` to the request body before it is signed and accepts
the response only when `cfetch_execution.scope_id` equals that requested
scope. Thus a valid CPU package response cannot masquerade as completion of an
NPU attempt. An explicitly configured remote endpoint is not part of this
fallback chain and receives no local requested-scope selection. Its admitted
scope must instead bind `transport: remote-attested`; transport identity is
never inferred from a copied scope ID or provenance string.

`release/inference-backends.json` is the machine-readable composition
boundary: every future local package maps its OS, architecture, and device
families to ordered admitted scope IDs and exact artifact recipes. That list is
empty while no scope is admitted; endpoint-only release variants remain
truthfully remote rather than borrowing candidate accelerator names.

The Rust core now validates and dispatches that plan, including supervised
process lifetime, expected-scope request binding, NPU-to-GPU-to-accelerated-CPU
fallback, one crash restart, and cached successful selection. With the current
empty `local_packages` array it selects nothing and preserves explicit endpoint
behavior. An admitted CPU endpoint alone is still not a valid local package.

The admission tooling uses a complete hash-locked transitive Python
environment. Final package conformance challenges the immutable target payload
with fresh nonces and re-executes every sequence bucket and wire grouping from
1 through 64 against retained canonical INT8 fingerprints. The two-phase
transaction stages content-addressed evidence and package bytes first; only a
complete set of conformance receipts may produce activation files. Empty
registry CI remains a no-op, not evidence.

The registry currently has no admitted backend and no local package. Nothing
in this candidate protocol certifies a target until those boundaries and a
real NPU/GPU/accelerated-CPU cohort pass.

The same protocol also gives every platform a common accelerated CPU/GPU floor
while target NPU packages are certified. Adding another device family means
implementing this small adapter and submitting its output cache to the global
all-pairs and mixed-store gates; it does not require changing cfetch's vector
store or training a new embedding model.
