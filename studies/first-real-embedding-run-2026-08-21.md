# First real embedding run — the whole brain, a real model (2026-08-21)

Question: semantic recall had only ever been exercised against a canned
endpoint on one host. Does it work against a real embedding model over the
real corpus, and are the design's load-bearing assumptions true in practice?

Setup: the whole brain tree (506 documents, 17,610 blocks; plus 9,475 code
files and 251,423 symbols, scanned in 12.8 s). Model `qwen3-embed-8b` served
by llama-broker (llama-swap) on the RX 6800, reached over a loopback forward
from the storage host. Stored at the shipped default: 1024 dimensions, f16.

## Results

**It works, and the derive-once design holds.** 17,610 blocks embedded in
~27 minutes at 655 blocks/min, producing a 34 MB shared artifact plus a
1.1 MB hash index in `state/cfetch/vectors/`. Coverage reads
`17610/17610 blocks embedded — complete`.

**The endpoint ignores `dimensions`, so client-side truncation is
load-bearing, not a nicety.** The request asks for 1024; the server returns
its native 4096 every time. Qwen3 is Matryoshka-trained, so cfetch truncates
to the 1024 prefix and re-normalizes — exactly the path the design predicted
it would need. Verified on the written artifact rather than assumed: sampled
records across the file have L2 norms of 1.00001, 1.00001, 0.99999, 0.99998
(f16 rounding), all 1024 components non-zero, and the file length is an exact
multiple of the 2048-byte stride with zero remainder.

**Content-hash keying deduplicates measurably.** 17,610 blocks produced
16,596 artifacts: 1,014 blocks (5.8%) are byte-identical to another block
somewhere in the tree and share its vector. The index carries one unique
hash per line with no duplicates.

**Embedding queries and documents identically was a real defect.** Retrieval
embedders of this generation are asymmetric — trained with an instruction on
the query and raw text on the document. cfetch was embedding both the same
way. Measured against a relevant block and a deliberate distractor:

| query form | cos(relevant) | cos(distractor) | margin |
|---|---|---|---|
| raw | 0.5298 | 0.4475 | +0.0823 |
| with the documented `Instruct:`/`Query:` wording | 0.4487 | 0.3210 | **+0.1276** |

The absolute cosines fall — the instruction moves the query vector — but
ranking only cares about the margin, which widens by 55%. On the live corpus
the effect was not subtle: for "what stops one machine from showing me an
answer that is already out of date", the unprefixed top-3 was unrelated
(a project note, a backup skill, a reimage log); the prefixed top-3 was the
memory about deleting stale claims and AGENT.md's own "stale memories
mislead". Fixed by `embeddings.query_prefix`, which applies to queries and to
nothing else — a document-side prefix would change every stored vector and so
would have to join the artifact identity first.

**Lexical is not the weak baseline.** For "never move data between
snapshotting filesystems with file-level tools", BM25 returned the two exact
governing rules and semantic did not. The query shared vocabulary with the
target, which is precisely the case lexical retrieval is best at. This is the
argument for `--hybrid` and for reranking, and against ever making semantic
the silent default.

## Operational finding

One llama-swap broker cannot serve the embedding model and the reranker
without thrashing: enabling both pointed at the same broker made a single
hybrid query swap models, and the query embedding hit the 10 s default
timeout. It degraded correctly — `semantic: query embedding failed (timeout)
— answering lexically` — which is the designed behavior, but the deployment
lesson is that the two stages want either separate endpoints or a generous
timeout.

## Not yet answered

Production wiring. The run went through a manual loopback forward because the
SSRF guard requires https for any non-loopback host and `llama-broker` has no
pinned ClusterIP; see the cfetch STATUS blocker.
