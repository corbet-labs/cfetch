# Retrieval readiness gates

cfetch has two different kinds of retrieval proof. They must not be confused.

1. The temporary retrieval fixture proves that the configured runtime can
   answer now. It checks BM25, vector output, semantic ordering, hybrid fusion,
   optional reranking, graph expansion, and reported execution placement.
2. The embedding admission suite proves that one exact model package is safe
   to join the shared vector space. It covers the full pinned corpus,
   cross-backend pairings, mixed-producer stores, sequence buckets, batching,
   repeatability, placement, latency, memory, and retained evidence.

The small fixture is a smoke test. It can reject a broken runtime, but it can
never admit a model or hardware package.

## Run the gates

The report-only commands always print what happened. Add one or more
`--require` options when the command should also act as a gate:

```console
$ cfetch retrieval-fixture --require vector
$ cfetch retrieval-fixture --require hybrid --require graph
$ cfetch doctor --deep --require production
$ cfetch doctor --deep --json --require production > retrieval-report.json
```

A report-only run exits successfully even when a stage is unavailable, because
its job is diagnosis. A required run exits nonzero unless every check selected
by that requirement says `pass`. The JSON report is printed before the exit
code is decided, so CI can retain the evidence from a failed run.

Available requirements:

| Requirement | Checks that must pass |
|---|---|
| `bm25` | Deterministic keyword control |
| `profile` | Canonical shared-vector profile is active |
| `vector` | Complete vector output and the semantic contrast |
| `hybrid` | Vector checks plus BM25/vector RRF |
| `reranker` | The configured reranker returns a complete reordered shortlist |
| `graph` | The known wikilink is followed after ranking |
| `local-acceleration` | Vector output plus reported use of a local NPU, GPU, or accelerated CPU package |
| `production` | Every production-required check below |

Options are repeatable and may also be comma-separated.

## What each check means

The fixture contains a keyword trap, a meaning-based answer, and one known
wikilink. It uses only temporary Markdown and a temporary index.

| Check | Pass condition | Why it exists |
|---|---|---|
| `bm25` | The keyword trap and linked recovery note appear in the fixed lexical order. | Proves keyword search really ran. |
| `profile_admission` | The configured model is the canonical model and its lifecycle is `active`. | Stops a custom or candidate model from looking production-ready. |
| `vector_output` | The route returns one valid query vector and one valid vector for every fixture note. | Proves an endpoint or local model answered with the required shape and non-degenerate values. |
| `semantic_ranking` | The meaning-based rollback note ranks above the keyword trap. | Proves vectors change ranking for a semantic reason, not merely that numbers were returned. |
| `hybrid_fusion` | The three-result RRF shortlist contains evidence supplied by both BM25 and vector ranking. | Proves both ranked lists participate in fusion. |
| `reranker` | When configured, it returns a result for every shortlist item. | Proves the optional cross-encoder stage answered. |
| `graph_expansion` | Expansion follows the fixture's exact wikilink. | Proves graph enrichment works while keeping it separate from scoring. |
| `local_acceleration` | The successful embedding call reports a local admitted backend and an NPU, GPU, or accelerated CPU device class. | Stops hardware discovery or a loopback URL from being presented as actual accelerated execution. |

The `production` requirement includes every row except `reranker` when
reranking is disabled. If reranking is configured, it becomes required: a
broken optional feature must not produce a green result.

Graph expansion is deliberately not a third ranking score. The ranking path is
BM25 plus vector RRF, followed by optional reranking. Wikilinks are expanded
after that ranking is complete.

## Hardware and model admission

`local_acceleration` only checks the runtime evidence from this call. Production
packages still have to pass the stronger admission contract in
`experiments/embedding-profile/cross_backend_eval.py` and be present in
`release/inference-backends.json`. Discovery, successful compilation, or one
plausible vector is never enough.

The canonical EmbeddingGemma profile is currently `candidate`, and the local
package registry is empty. Therefore `--require production` is expected to fail
until at least one real target package passes the full admission suite and the
profile is activated. This is a useful red gate, not a fixture error.

The machine-readable fixture contract is schema version 2. Every check has a
stable `id`, a `pass`, `fail`, or `not_run` status, a plain explanation, and the
evidence used for that decision.
