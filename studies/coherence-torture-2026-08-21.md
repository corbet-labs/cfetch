# Coherence torture — real-fleet results (2026-08-21)

Question: do the serving daemon's coherence guarantees (drain barrier,
generations, read-your-writes) hold on real hardware — specifically with
writers on NFS clients and recalls over the TCP-served path?

Setup: serving daemon (branch wt/serve, 228-test suite green) on the storage
host, scratch brain under a tracked tree path; writer + none-tier client on a
laptop over NFS; second concurrent writer on an LXC host via rbind. Recall =
TCP, bearer token, barrier path.

Results:
- Single writer, write→immediate recall, 200 iterations: 0 misses, 0 stale
  answers. Latency p50 51 ms, p95 52 ms, max 54 ms.
- Dual-host concurrent writers, 100 rounds (2 writes + 2 recalls each):
  0 misses. Per-recall p50 31 ms, max 32 ms.
- Server-side inotify OBSERVES NFS-client writes (nfsd goes through VFS
  fsnotify): generation advanced 1:1 with remote writes. The "only the 60s
  fingerprint bounds NFS staleness" caveat is refuted for the
  storage-host-serves-its-own-disk topology.

Experiment bug worth remembering: the first run pointed the scratch brain
under `logs/`, which the tree's gitignore excludes and the walker honors —
generations advanced while every scan indexed zero docs. Torture your test
in a tracked path; the exclusion behavior itself is correct for production.

Verdict: read-your-writes across hosts holds with zero tolerance at ~50 ms
end-to-end (dominated by process spawn + TCP round trips, not the barrier).
The catalog's internal form (SQLite FTS5 in-process) is not observable in
any measurement — coherence is a property of the barrier/generation design,
not of the storage engine.
