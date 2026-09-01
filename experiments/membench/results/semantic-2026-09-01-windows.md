# Semantic recall benchmark — 2026-09-01, Windows

Environment: AMD Ryzen 9 5900X (12C/24T), 64 GB RAM, NVMe SSD, Windows 11.
Node 24.14.0. cfetch 0.9.9 (source build @ 5cc7213, endpoint-model branch).

Embedding: Ollama 0.33.2 serving `nomic-embed-text` (137M params, 768-dim,
Q4_K_M) on `127.0.0.1:11434` with `OLLAMA_KEEP_ALIVE=-1`.
LM Studio was available but NOT used for embeddings (see the root-cause
section below).

Brain: 47 docs, 3,194 blocks, 1,623 unique content hashes embedded.
Vector store: 1.29 MB (INT8 × 768-dim packed format).
Index database: 4.6 KB.

## Latency (5 queries × 5 runs each)

| Tool | Mode | Avg | Min | Max | What's in the path |
|---|---|---|---|---|---|
| cfetch | lexical | 30 ms | 24 | 38 | binary start + SQLite FTS5 |
| cfetch | semantic | 71 ms | 63 | 76 | binary start + Ollama embed + INT8 cosine |
| openwolf | lexical | 104 ms | 98 | 128 | Node.js start + SQLite |
| openwolf | semantic | 155 ms | 147 | 165 | Node.js start + LM Studio embed |

cfetch's semantic path is 2.2× faster than openwolf's, and its lexical path
is 3.5× faster. The semantic overhead (embedding + cosine) adds ~41 ms over
lexical — the embedding call itself is ~38 ms and the INT8 cosine ranking
over 1,623 vectors is ~3 ms.

## First-run (cold start) indexing

| Operation | Time |
|---|---|
| `cfetch scan` (full re-index) | ~1.5 s |
| `cfetch embed-index` (1,623 blocks) | 42 s |
| `cfetch scan` (cached, no changes) | 109 ms |

The 42 s embed-index is dominated by the per-batch API round-trips to
Ollama (64 items per batch, ~25 batches, ~1.6 s per batch including HTTP
overhead). The cosine computation itself is negligible.

## Hardware load

| Component | RAM | CPU (idle) | CPU (during query) |
|---|---|---|---|
| Ollama (server + model) | ~105 MB | 0% | <5% of one core |
| cfetch binary | ~30 MB (exits after query) | 0% | <1% for 80 ms |
| Vector store | 1.29 MB on disk | — | memory-mapped |
| **Total semantic stack** | **~140 MB** | **~0%** | **<5% for <100 ms** |

## Extrapolation to weaker hardware

| System | RAM | Embedding latency | Semantic recall | Notes |
|---|---|---|---|---|
| 4-core laptop, 16 GB | 140/16,384 = 0.9% | ~80-120 ms | ~120-160 ms | Comfortable |
| 4-core laptop, 8 GB | 140/8,192 = 1.7% | ~120-200 ms | ~160-250 ms | Workable |
| Mini PC (N100), 8 GB | 140/8,192 = 1.7% | ~300-500 ms | ~350-550 ms | Acceptable for CLI |
| Raspberry Pi 5, 8 GB | 140/8,192 = 1.7% | ~500-1000 ms | ~550-1050 ms | Batch indexing slow; queries usable |

The 137M-parameter model is small enough that CPU inference is sub-100ms on
any modern x86-64 core. The vector store grows linearly with content
(~0.8 KB per block at INT8), so 10,000 blocks ≈ 8 MB — negligible.

## Root causes found during benchmarking

1. **IPv6 localhost penalty (2,100 ms)**: `localhost` resolves to both `::1`
   and `127.0.0.1` on Windows. ureq (Rust) tries IPv6 first, gets connection
   refused (most local servers bind IPv4 only), waits for the TCP timeout,
   then falls back to IPv4. Fix: use `127.0.0.1` in the endpoint URL.

2. **LM Studio per-connection model unload**: LM Studio unloads the embedding
   model when the last HTTP connection closes. Node.js's undici creates
   lingering TIME_WAIT connections that keep the model warm; Rust's ureq
   closes cleanly and the model is reloaded on every CLI invocation.
   Fix: Ollama with `OLLAMA_KEEP_ALIVE=-1` never unloads.

3. **Model name mismatch**: serving layers rename models (LM Studio prefixes
   `text-embedding-`, Ollama uses short names). cfetch's profile validation
   requires the canonical name. Fix: the `endpoint_model` config field sends
   the serving layer's name while validating against the canonical one.

## Raw data

See `semantic-speed.csv` in this directory for per-query measurements.
