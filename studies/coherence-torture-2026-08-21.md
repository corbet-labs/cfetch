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

## The barrier's ordering premise is inotify-specific (macOS, 2026-08-21)

The drain barrier claims: *observing sentinel N implies every write that
completed before the barrier began has been counted*. That holds because the
sentinel rides the SAME event queue as the writes — under **inotify**, which
delivers events in FIFO order.

**macOS FSEvents makes no such promise.** It coalesces per directory, batches
with latency, and does not order events across directories. The CI matrix
caught the consequence on macos-latest: a concurrent-writer run produced an
answer labeled `fresh: true` that was missing a committed statement —
precisely the zero-tolerance invariant. It passed the previous macOS run, so
the failure is intermittent, which is the signature of a violated ordering
assumption rather than a broken mechanism.

Consequence for the design: the fast sentinel path is a LINUX optimization,
not the guarantee itself. On platforms without ordered delivery the barrier
must establish coverage a different way (a stat-fingerprint taken at query
start, which the daemon already computes for its 60s backstop), accepting a
higher per-query cost where correctness demands it.

The honest framing: cfetch's freshness guarantee is only as strong as the
platform's event ordering, and the code must know which platform it is on
rather than assuming the strongest one.

## Two barrier modes, and what each costs (2026-08-21, same day)

The fix: the watcher backend declares an ORDERING CAPABILITY, read from
`notify::Watcher::kind()` at runtime rather than guessed from `target_os`
(notify's macOS backend is selectable at compile time, so the OS does not
settle the question). Inotify is the only `ordered` row; FSEvents, kqueue,
Windows `ReadDirectoryChangesW`, polling and anything future are `unordered`,
because an unrecognized backend wrongly called ordered is a silent-staleness
bug while one wrongly called unordered only costs latency.

Ordered keeps exactly the sentinel path. Unordered proves coverage by CONTENT:
the barrier takes the stat fingerprint the daemon already computes for its 60s
backstop, then waits until the applied catalog covers it — either the committed
catalog's own fingerprint IS the entry fingerprint (quiescent tree: no wait at
all), or a stat walk that BEGAN after the entry fingerprint was taken has
committed (concurrent writers: that walk saw a superset, because writes only
move forward). The second clause is what makes the mode usable under load; the
first is what makes it cheap when nothing is happening. A query on this path
also asks the rebuild worker for an immediate fingerprint pass and skips the
debounce, because an unordered watcher may batch this query's writes past the
barrier budget or coalesce them away entirely.

Measured on the build host, debug build, against a real brain tree (scratch
state dir; the live daemon untouched):

| what | ordered | unordered |
|---|---|---|
| quiescent recall over the real tree (p50 / p95) | 1.9 / 2.1 ms | 50 / 58 ms |
| write -> recall on a real-shaped tree, 30 rounds (p50 / max) | 84 / 98 ms | 104 / 122 ms |
| zero-tolerance misses | 0 | 0 |

The entry fingerprint itself is the whole difference: 46 ms p50 / 61 ms max over
a real brain (92 indexable directories). No scoping was needed and no
honest-unfresh fallback was needed — the cost fits the 5s budget with two orders
of magnitude to spare, and none of it lands on the ordered path.

The measurement did buy one design change. Over a deliberately unpruned tree —
313,527 indexable directories, 27k markdown files — the same walk costs **13.5 s
p50**. Taking that fingerprint would blow the barrier's own bound on every
query, and BOUNDED is the older promise. So the worker publishes what its 60s
backstop walk cost, and an unordered barrier that cannot afford the walk answers
immediately with `fresh: false` and a note naming both numbers. The catalog still
converges underneath on the backstop cadence: the answer is stale-and-labeled,
never silently stale, and never a hang.

Testability was the other requirement. The unordered path is not reachable on
Linux CI by `cfg` alone, and a path only macOS runs is a path only macOS debugs
— so `CFETCH_BARRIER_MODE=ordered|unordered` forces either one, and the
zero-tolerance concurrent-writer torture now runs twice on Linux: once on the
platform's own mode, once forced onto the unordered path. `serve-status` and
`cfetch status` name the mode in force, and a serving host too old to report one
reads as "mode not reported" rather than as the fast path.
