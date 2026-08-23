# Changelog

Notable changes per release. cfetch is young and moving fast; anything not
listed here is a fix or an internal change with no effect on behavior.

## Unreleased

## 0.9.8

## 0.9.7

## 0.9.6

## 0.9.5

- **crates.io distribution.** The verified v0.9.4 source release is available
  as `cargo install cfetch --locked`; future tags publish through short-lived
  GitHub OIDC credentials after the complete platform release succeeds, with
  no registry token stored in GitHub.
- **Platform-stable resident budgets.** Crowded resident indexes stop repeating
  long absolute brain-root prefixes on every line, so macOS and Windows keep
  the same hard digest cap as Linux without dropping configured files.
- **Windows self-read accounting.** PowerShell drive paths retain their
  backslash separators when cfetch recognizes safe whole-file shell reads,
  keeping those reads in the self-read ledger instead of silently losing them.

## 0.9.4

- **Ring-aware resident budgets.** Resident digest space is water-filled by
  ring-derived or explicit entry weight. Short entries return unused space to
  longer ones, ring-0 invariants outrank less load-bearing material, and the
  configured digest budget remains a hard cap.
- **Truthful Arch package identity.** The AUR build and check phases now use
  the same catalog variant, so `cargo test` cannot overwrite the packaged
  binary with an unidentified developer build. A CI contract ties both Arch
  architectures and both package phases back to the executable release
  catalog.
- **Unix-friendly output pipes.** Normal consumers such as `head` may close a
  pipe early without making cfetch print a Rust panic or fail the command.
- **Release plumbing maintenance.** Homebrew reads the published release
  manifest alongside its checksums instead of assuming a source checkout, and
  GitHub artifact transfer uses the current Node 24 actions.

## 0.9.3

- **Executable release catalog.** `release/variants.json` is now the single
  source for runtime advice, CI, release archives, and Homebrew. Artifact names
  explicitly say `remote` while inference remains endpoint-only; the empty
  backend Cargo features and nonexistent silicon recommendations are gone.
  `cfetch variants` reports the embedded catalog, and `cfetch hardware` can no
  longer recommend an artifact that does not exist.
- **ARM release coverage.** Linux and Windows ARM64 join Linux, macOS, and
  Windows x86_64 in the generated release matrix. Every catalog entry gets a
  blocking compile job before a tag can become a complete release.
- **Transactional patch releases.** A manual release action now computes the
  next pre-1.0 patch, updates every version and license-notice surface, pushes
  the preparation commit, waits for the complete blocking CI matrix, and tags
  only that verified commit. The tag then drives archives and Homebrew; Crow
  independently reconciles the AUR package.

## 0.9.2

- **Homebrew install.** `brew tap corbet-labs/cfetch && brew install cfetch`
  on macOS (Apple silicon and Intel) and Linux x86_64. Releases now publish
  `checksums_sha256.txt`, and the tap's formula is regenerated from those
  published checksums on every tag rather than hand-written. There is no
  linux-arm64 build, and the formula says so instead of offering an x86_64
  tarball that cannot execute.

- **Multi-harness adapters.** The official `rmcp` SDK now owns MCP framing and
  negotiation, `agent-session` normalizes Claude/Codex/Gemini/Cursor transcript
  discovery, and `agent-config` installs cfetch's confirmed MCP, instruction,
  and native-hook surfaces across its 25-harness registry. The libraries are
  exact-pinned behind cfetch-owned compatibility facades; cfetch 0.9 marker
  formats are converted once without leaving parallel legacy registrations.
  `cfetch install --project <path>` exposes confirmed project-only surfaces.

- **Dependency-license compliance.** CI now rejects unreviewed licenses and
  verifies a generated third-party license/NOTICE bundle. Release archives and
  the Nix and Arch packages all install that bundle beside cfetch's own license;
  copied third-party material requires an explicit provenance record.

- **Authenticated iroh transport.** Invites now carry the origin's real iroh
  endpoint address, remote redemption binds a grant to the QUIC-authenticated
  joining endpoint, and `recall --slice` plus citation expansion route through
  the joining daemon without persisting the one-time secret. Citation results
  are filtered against the granted slice before they cross the network.
- **Pre-1.0 release lock.** The crate build, blocking CI, and release preflight
  all reject a 1.x package version. A release tag must also match Cargo.toml.
  The lock stays in place until the operator explicitly authorizes 1.0.
- **Codex output condensation.** Oversized Bash listings now use Codex's
  PostToolUse model-feedback replacement path instead of adding a second copy
  as context. The complete original is preserved in private local state and
  linked from the condensed result under a 64 MiB retention cap; test and build
  output remains untouched.
- **Cross-platform state locking.** Identity creation now reads the key only
  while holding its creation lock, so Windows cannot observe a winner's
  zero-byte in-progress file. Lock retries honor their full deadline, and
  concurrent session updates have enough bounded time on macOS and Windows.

- **Engine selection.** A variant is now a Cargo feature selection rather
  than a source fork: which engines are compiled in is a build-time fact,
  which device to use is a run-time one, and `cfetch hardware` reports both
  plus what it will actually do. A device no compiled-in engine can drive is
  skipped rather than selected and then failed on, and everything falls back
  to the remote endpoint — which is the right answer for a host that holds
  nothing.
- **Hardware detection.** `cfetch hardware` reports the accelerators it can
  see, what proved each one, and the variant this machine should run, under
  the `<os>-cfetch-<silicon>[-<level>]` scheme. The policy is NPU > GPU > CPU
  and it is encoded as an ordering rather than as a comment — an NPU is
  preferred even where the GPU beside it is faster, because it draws less
  power and is the one processor nothing else on the machine is competing
  for.

- **Document-side embedding prefixes, as part of the artifact identity.**
  `embeddings.document_prefix` carries the instruction models like E5 and
  EmbeddingGemma expect on documents. Because it changes every stored vector,
  it is keyed into the shared artifact's filename and recorded exactly in its
  header — two hosts configured differently write separate files rather than
  appending incompatible vectors to one. Stores written before this exist
  unchanged and keep working: no prefix line means raw documents, which is
  what they hold.
- **Invites and grants.** `cfetch invite <slice>` mints a one-time ticket,
  `cfetch join <ticket>` redeems it, `cfetch grants` shows who holds what.
  A ticket is an address plus a secret, NOT an authorization: the origin looks
  the secret up in its own records, so a forged ticket buys nothing and a real
  one can be pasted through any channel. The origin stores only the secret's
  hash. An invite binds to the first host that redeems it, a retry from that
  same host is not an error, and a second host is refused. Inside one storage
  group — hosts sharing the tree — redemption needs no network at all.
- **Host identity.** Each host now has a persistent iroh keypair, created on
  first use, whose public half is the endpoint id peers will grant slices to.
  `cfetch identity` prints it and `cfetch status` names it. The key lives in
  the per-host state directory and never in the brain tree — a shared tree
  would hand every host the same identity, making a grant to one machine a
  grant to all of them. A key that cannot be read is an error rather than a
  fresh identity, because regenerating would orphan every grant naming the old
  one.

## 0.8.0

- **Slices.** Named prefix sets over the tree, nestable, with `cfetch slices`
  and `cfetch recall --slice`. A document belongs to its innermost slice; a
  query for a slice reaches everything inside it; an unknown name is refused
  rather than widened to the whole tree. Membership is derived from the path
  rather than stored, so it cannot fall out of step with the configuration.
  A brain with no slices behaves exactly as before.

- **The drain barrier is sound on every platform, and says which path it is
  on.** Proving coverage with a sentinel works only where the watcher delivers
  events in order, which is inotify and nothing else; macOS FSEvents coalesces
  per directory and does not order across them, and a concurrent-writer run
  there could answer `fresh: true` while missing a committed statement. The
  watcher backend now declares its ordering capability at runtime: Linux keeps
  the sentinel path unchanged, and every other backend proves coverage by
  comparing a stat fingerprint of the tree, taken at query entry, against what
  the committed catalog has seen. `serve-status` and `cfetch status` name the
  mode in force. Where one such walk would cost more than the barrier's own
  budget, the answer comes back immediately as stale-and-labeled rather than
  blowing the bound.

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
- **Queries are embedded the way retrieval models expect.** `embeddings.query_prefix`
  carries the instruction modern embedders are trained to see on the query
  side, and only there — documents stay raw, so stored vectors are unchanged.
  Without it cfetch was embedding queries and documents identically, which
  these models are explicitly not trained for.
- **A host that holds nothing can still rank semantically.** `--semantic` and
  `--hybrid` used to refuse over remote serving, because the querying host had
  no vectors and no endpoint to embed its own query against. The serving host
  now ranks on the client's behalf — it is the one holding both — so the
  none-tier hosts the whole design is for are no longer second-class. Ranking
  itself moved into one shared pipeline, so the same query against the same
  tree ranks identically whether it was answered locally or over the wire, and
  any degradation travels back with the answer.
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
