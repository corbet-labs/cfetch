# Changelog

Notable changes per release. cfetch is young and moving fast; anything not
listed here is a fix or an internal change with no effect on behavior.

## Unreleased

- **Rings are configuration, not code.** `ring_rules` (ordered prefix → ring)
  and `exclude_prefixes` replace a hardcoded taxonomy. The shipped defaults
  reproduce the previous behavior exactly. Secrets, logs and `.git` remain
  excluded by a boundary no config can lift.
- **Injection is a policy.** A resident entry may declare
  `scope: { hosts, repos }`; only matching sessions receive it, and skipped
  entries cost no budget and are reported rather than silently dropped.
- **Windows and macOS are supported platforms.** A local-channel abstraction
  (unix socket / loopback TCP with a per-daemon token), platform-native paths
  and quoting, and a lock implementation per platform. Linux, macOS and
  Windows all gate the build — a failure on any of them blocks. Served paths
  are canonicalized in the process, so a repository answered by a Windows host
  and by a Linux host returns identical lines rather than the same file under
  two spellings.
- **Semantic recall works on a living brain.** Vectors are keyed by content
  hash instead of a volatile row id, so editing one file no longer discards
  every embedding; they are stored as shared f16 artifacts (dimensions
  configurable) that any host can read and only a capable host computes.
  Partial or missing coverage is reported, never silently answered lexically.
- **Rings 5 and 6 live in the tree.** Session exhaust and the cost ledger are
  versioned append-only JSONL streams per host; staging candidates are
  markdown files with content-addressed ids, so a candidate flagged on one
  machine is visible — and drainable — from any of them.
- **Cross-encoder reranking.** An optional second stage reorders a retrieved
  shortlist by reading query and document together, which no retrieval scorer
  does. Recall widens to `rerank.candidates` and the answer is cut back
  afterwards, so the reranker can only promote what retrieval proposed. Off by
  default; every failure answers in retrieval order with the reason attached.
- A serving daemon now indexes its own code roots on startup instead of
  waiting to be asked, `map` is answerable remotely, and the dashboard routes
  to its serving host rather than opening a local index it does not have.

## 0.7.0

- **Serving mode.** The machine holding the markdown runs a daemon that
  watches its own tree and answers queries behind a drain barrier
  (serve-fresh-or-wait, bounded); every answer carries its origin, catalog
  generation and a `fresh` flag. Other machines can hold nothing at all and
  query it. Read-your-writes across hosts, measured at zero misses over 300
  cross-host checks.
- Watch scope derived from the indexer's own walk: no symlink following, no
  watching what is never indexed (a real tree went from ~524k required
  watches to 90), and the listener binds before registration completes so a
  restart is not an outage.
- The 26 findings of an adversarial design review, including a torn-ledger
  race, a hook that read whole files to count lines, and a staging queue that
  its own retention could delete.

## 0.6.0

- Reminder queue delivered on the next prompt instead of at stop (a stop-time
  injection costs an entire extra model turn), cadence re-injection of the
  top rules, and `cfetch audit` — the always-on context bill priced honestly,
  including cfetch's own injections.
- Recall deduplicates content mirrored between sources, keeping the
  highest-trust copy and naming the others.

## 0.5.0

- Automatic capture (ring 6), promotion traps and the ring-5 staging queue.
- Measured token accounting from the session transcript, booked as per-turn
  deltas.
- Import-graph importance, `cfetch map`, background code scanning.
- Packaging: nix flake, Arch package, CI and release automation.

## 0.4.0 and earlier

Recall over the ring model with content-addressed citations, the tree-sitter
code index, hook-based injection, the MCP server, and the terminal dashboard.
