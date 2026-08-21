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

## Watcher scope — three defects found by DEPLOYING (2026-08-21, same day)

A declarative rollout attempt on the real fleet found what no unit test had:

1. **Symlink escape**: the recursive watch followed a wineprefix
   `dosdevices/z: -> /` inside a checkout and walked the entire root
   filesystem.
2. **Watching what the indexer ignores**: `projects/` and gitignored scratch
   dumps pushed the requirement to ~524k watches against a 524,288 default.
3. **Bind-after-registration**: the TCP listener came up only after full watch
   registration (minutes), making every daemon restart a fleet-wide recall
   outage once desktops are none-tier.

Fix: watches are now derived from the INDEXER's own walker (same `ignore`
settings, same exclusions, `follow_links(false)`), registered one
non-recursive watch per indexable directory, in a BACKGROUND thread — and
`settled` (hence freshness) is withheld until registration completes, so an
answer served during startup is labeled stale rather than wrongly fresh.
New directories are picked up on the 60s backstop cadence.

Measured after the fix, against the real brain tree:
- inotify watches: **90** (was ~524k required)
- TCP listener bound: **< 0.5 s** after start (was minutes)
- live write -> recall visible in ~2 s; deletion likewise
- recall latency on the served tree: 23 ms

Lesson worth keeping: the deployment attempt was the test. Three defects,
zero of them reachable from a temp-dir fixture.

## v2 architecture live on real hardware (2026-08-21, evening)

The serving cutover converged: the storage host runs the serving daemon over
the shared tree, and a none-tier client on another machine holds no index at
all and answers every query over the overlay network.

Measured on the live fleet:

- serving host: unit active, TCP listener up, **90 inotify watches** for the
  whole brain (the pre-fix recursive watcher demanded ~524k and escaped
  through a symlink), bind < 0.5 s after start.
- none-tier client: `status` reports "recall/find/expand route to <host>
  (none-tier; no local index)"; a cross-host `recall` returned in **34 ms**
  with the footer `served by <origin> (generation 1, fresh)`.
- Every answer carries {origin, generation, fresh} — freshness is a claim the
  client can see, and staleness would be labeled rather than silent.

This is the PRD's three-tier holding model working as specified: storage +
metadata + serving on one host, nothing at all on the other, and the drain
barrier making the remote answer as fresh as a local one.
