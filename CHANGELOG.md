# Changelog

Notable changes per release. cfetch is young and moving fast; anything not
listed here is a fix or an internal change with no effect on behavior.

## Unreleased

- **Round 4: the last three findings (#35), closing the 2026-08-31 bug
  hunt at 22/22.** The model's output budget (16384 tokens) now gates at
  submit time — a proposal whose `after` bytes exceed what the model can
  echo is refused with a clear split-or-apply-manually message, instead of
  truncating, failing the JSON parse, and retrying forever. Evidence
  coverage holds across late arrivals: when raw events rotated out at
  submit time (candidate-record citation was sufficient) and new matching
  events arrive during the review round-trip, the fallback stays
  sufficient at apply time — the proposer cannot cite events it has never
  seen. And stream appends hold a per-stream advisory lock across
  rotate+open+write, closing the race where a concurrent writer's
  rotation between our rotate check and our open landed our line in the
  wrong generation. Issue #27 is closed.
- **Round 3 of the 2026-08-31 bug hunt (#32, #33): boundaries and
  consistency.** `ensure_fresh` refuses to serve a never-scanned empty
  catalog instead of silently answering every query with zero hits;
  same-line symbol containers resolve to the innermost scope; the recall
  candidate pool expands on demand so a pathological block with many copies
  cannot shrink the result below the limit; Windows brain-root matching is
  case-insensitive (8.3 names, subst drives, symlinked homes no longer
  blind the hot-file trap); `cards list`/`status` hold the same store lock
  as the mutators; doctor reuses the pre-computed daemon probe instead of
  issuing its own; doctor and the statusline agree on maintenance priority
  (exception before model-unavailable); and `daemon status`'s durable
  observation after a SIGKILL is documented as the designed repair path,
  not an invariant violation.
- **Round 2 of the 2026-08-31 bug hunt (#29, #30): accuracy and robustness.**
  Nested `ring:` frontmatter keys no longer self-promote (only unindented
  top-level declarations count); ``` `bash` ``` inside a fence no longer
  closes it (CommonMark trailing-text rule); self-wikilinks stop inflating
  the unresolved-references metric; hyphenated query terms split into
  independent prefix terms (`state-machine` finds "the machine's state");
  rerank refuses an omitted `index` field (matching the embeddings client);
  the `query_for` 12-term cap now actually caps across payload keys; adapter
  launch failures retry instead of poisoning the process cache; torn vector
  index tails (crash between `writeln!`'s two writes) are repaired on the
  next open instead of corrupting two records; session-state GC covers
  crash debris (`.tmp.<pid>` and `.lock` files); `failure_history` redacts
  its query so a pasted failing line with a real secret can actually find
  its stored (redacted) signature; and `hardware.rs` detects MOVBE for the
  v3 verdict as its own comment specifies.
- **Ring-6 traps no longer record healthy commands as failures.** Any
  non-empty `stderr` used to mark a bash event `failed` — but `git push`,
  `npm install` and `pip` stream progress to stderr while exiting 0, and
  every trap (fix-discovered, recurring-failure, failure history) keys on
  that field. Stderr now signals failure only when the response carries no
  explicit success marker (`is_error:false`, `exit_code:0`), keeping the
  legacy-harness path the branch existed for. Found by the 2026-08-31 bug
  hunt, along with everything below in this block.
- **The embed queue cannot be frozen by one bad block.** The pending set is
  ordered and a row-level refusal (degenerate or wrong-width vector) aborted
  the batch before any write, re-selecting the identical head forever. A
  failed batch now retries its rows individually — refused rows are skipped
  with a note and retried next run — and a fully-refused batch bails with a
  resumable error instead of looping.
- **A corrupt vector record no longer bricks group semantic recall.** One
  flipped byte in the shared store hard-failed every hydrate; the read side
  now skips invalid records (they stay "missing" and re-embed) while
  write-side validation stays strict.
- **Maintenance reject is idempotent and submit no longer resurrects.** The
  autonomous loop re-derives deterministic proposal ids; a rejected twin
  used to wedge its PENDING copy in place — two model calls and one
  exception event every cycle, forever. An identical terminal copy now
  means the decision was already made.
- **A UTF-8 BOM no longer hides the ring frontmatter.** Windows editors emit
  BOMs by default; the BOM made the `---` comparison fail, so a BOM'd
  `ring: 5` quarantine marker was invisible and the file indexed — the
  documented fail-closed contract inverted by encoding accident.
- **Unreadable files are visible, and incremental rescans keep their last
  good rows.** A read failure (invalid UTF-8, an editor's sharing violation)
  was invisible: the file stayed in the fingerprint claiming fresh, and the
  rescan path deleted existing rows without reinserting. Full scans report
  skips; a one-second editor lock must not delete a document.
- **Semantic and hybrid recall dedup native mirrors**, like lexical recall
  always did: a mirrored block has the same hash and the same vector and
  ranked adjacent to itself, doubling one statement and displacing a
  distinct block.
- **Redaction fixes: glued short-flag secrets redact themselves** (`mysql
  -psecret` used to keep the secret and redact the next argument), **and
  sessionless events no longer merge**: two harness windows without a
  session id shared one `unknown-session` key, cross-contaminating
  repeat-read state and disabling the cross-session traps.
- **Consume is permanent, dotted hosts cannot collide, and prefix flags find
  the program.** A consumed candidate moves to `dismissed/` (the permanent
  do-not-restage marker) instead of being deleted and resurrecting when its
  consume record left the trap window; host ids containing dots can no
  longer collide with the rotation suffix grammar (one host's live stream
  is another's rotated generation); `sudo -u nobody pytest` now finds
  `pytest` instead of eliding the failing assertion from the condensed log.
- **`src/bin` files resolve `use crate::` against their own crate**, not the
  library beside them — false edges on module-name collisions and missing
  edges otherwise; **`cards status` survives a store without an `origin`
  remote** (`git remote get-url` exits 2 on absence, which was treated as a
  hard error instead of the designed degraded report). Remaining findings
  from the hunt are tracked in #27.
- **The tree config is content-only.** `.cfetch/config.json` inside the brain
  is agent-written and cloned across machines, so it may now carry only
  content keys (rings, slices, resident entries, budgets): `embeddings`,
  `rerank`, `maintenance`, `serve`, `client`, and `brain_root` found there are
  a hard error naming the key, the machine-local file overlays the tree for
  everything it sets, and the root itself never comes from any file — the
  documented invariant, now enforced. Tree-layer resident and code-root paths
  must stay tree-relative; absolute paths remain a machine-local decision.
- **Quarantine locations ignore self-promoting frontmatter.** A file in a
  ring-5+ directory (staging, logs) that declares a lower ring is skipped
  exactly like one whose frontmatter was stripped: the location decides, and
  the "cannot be edited away file by file" promise now holds against a
  well-formed lie. Promotion below the location ring is unchanged.
- **The egress guard resolves hostnames before trusting them.** `check_endpoint`
  refuses any address a configured host resolves to inside the already-refused
  ranges (plus loopback), closing DNS-rebinding names and resolver spellings
  like `0x7f000001`; rerank responses gained the 2 MiB cap the other HTTP
  clients already had.
- **Edge-supplied ids and local surfaces hardened.** `cfetch staging`
  consume/dismiss validate their id as one filename segment, a grant can no
  longer be named `root` (config reserves the whole-tree slice), the unix IPC
  socket is pinned to mode 0600 after bind, hook stdin reads are bounded at
  64 MiB, and maintenance review reads gate their proposal id.
- **The TCP serving listener is bounded.** At most 64 unauthenticated
  connections are held before the token is known, mirroring the iroh accept
  loop's permit cap, and responses past the 16 MiB serving limit are refused
  instead of serialized whole — a tokened `limit: usize::MAX` no longer ships
  the entire catalog in one line.
- **Dependency advisories gate CI.** `deny.toml` gained an `[advisories]`
  section (`yanked = "deny"`, reasoned ignores for the two unmaintained
  transitive advisories), CI runs `cargo deny check advisories licenses`, and
  the yanked `chacha20 0.10.1` left the lockfile for 0.10.2.
- **A maximum-performance self-build profile.** `[profile.release-max]` adds
  fat LTO and single-codegen on top of `release`, for binaries that never
  leave the machine they were built on; `docs/self-build-performance.md`
  records when that pays and why shipped binaries stay portable.
- **One-line duplicate symbol uses no longer abort the scan.** Two calls to
  the same function written on a single line inside one container produce an
  identical `symbol_uses` tuple; the duplicate is dropped at the insert. Found
  by the new membench harness on lodash, where 7 of 48 JS files — `lodash.js`
  included — aborted every code scan.
- **membench: a reproducible memory-tool benchmark.** `experiments/membench`
  carries a four-arm agent battery (continuity, locate, bugfix, stale-memory
  poisoning) and a speed harness, with its first recorded run stamped and
  committed: cfetch vs openwolf-enhanced 1.28.1 on lodash, fastify, tokio, and
  a synthetic 300-module tree.

- **Atomic local admission release boundary.** One hash-locked command replays
  the current admitted cohort, stages bounded physical evidence, runs every
  exact target-package/scope conformance challenge, creates the activation
  bundle, and emits a content-addressed immutable publication plan. It performs
  no external mutation. After an operator publishes the planned assets, source
  activation downloads each one through credential-free GitHub HTTPS, confines
  redirects to GitHub, and verifies its exact size and SHA-256 before writing.
- **Explainable source dependency graph.** `cfetch code-graph path` returns one
  deterministic shortest import chain, while `cfetch code-graph impact` walks
  reverse imports for a bounded blast radius and `cfetch code-graph context`
  traverses both directions around one file. Context retains one deterministic
  shortest explanation edge per related file and counts limit omissions. Typed
  steps now retain exact source ranges. `cfetch code-graph symbol` adds
  parser-proven `contains`, direct `calls`, and type `references` edges,
  resolving them only through explicit imports to one unambiguous file-level
  definition. Ambiguous names fail closed, relative paths are stable across
  serving hosts, and equivalent read-only MCP tools use the same graph query
  pipeline.
- **Selective nixcards knowledge.** `cfetch cards` initializes and manages the
  public nixcards catalogue as a blobless sparse checkout at
  `knowledge/cards`, with dotted branch selectors, JSON status, explicit
  fast-forward sync, and an optional handoff to the nixcards TUI. Git's native
  sparse state is the sole local selection record shared by both tools;
  materialized cards inherit trust only from their canonical knowledge path.
- **Autonomous AI memory maintenance.** A configured daemon reacts to changed
  ring-5 evidence, runs bounded proposal and isolated review passes, rechecks
  deterministic authority, citation, path, secret, expiry, and exact-byte
  gates under a transaction lock, and applies Markdown without routine
  approval. Direct Obsidian edits win every race. Immutable activity and
  exception history, global and per-file pause controls, exact safe revert,
  manual debugging commands, and the terminal Activity pane keep the process
  inspectable without turning maintenance into a human queue.
- **Obsidian knowledge graph.** `cfetch graph` and the terminal Graph pane expose
  the rebuildable wikilink graph with focused neighborhoods, ambiguity-safe
  note resolution, ring and degree metadata, bounded JSON, and local, serving,
  or authenticated slice-scoped remote access. Markdown remains the only
  authoritative relationship store.
- **Continuous derived-state upkeep.** Every storage daemon watches direct
  Markdown edits regardless of whether it serves clients. Catalog generations
  rebuild lexical and graph views, while a resident vector worker hydrates
  compatible shared or authorized-peer artifacts before deriving only missing
  content hashes. Joined peers and newly appearing shared artifacts take effect
  without restarting the daemon.
- **Maintenance-aware diagnostics.** RuntimeStatusV1, Claude's status line,
  Codex transition notices, MCP status, `cfetch doctor`, and the terminal System
  pane distinguish maintenance configuration, local or remote route, proposal
  and review models, last attempt, paused/degraded state, candidates, outcomes,
  and exceptions without exposing endpoint or credential details.
- **One semantic and output ABI for v1.** The candidate v1 profile freezes the
  pinned EmbeddingGemma-300M source revision, tokenizer, retrieval prompts,
  full 768 dimensions, pooling/normalization, and the canonical 768-byte signed
  INT8 output/index codec. Core ML, LiteRT, OpenVINO, and other packages may
  use target-native artifacts and internal precision. No NPU or runtime is a
  numerical anchor; model-pipeline or codec changes require a new network
  major and coordinated re-embedding.
- **Absolute cross-device admission.** Every concrete artifact/runtime/device
  scope must be repeatable and meet the same fixed retrieval floors in every
  ordered query-backend x document-backend pairing, including self-pairs.
  Each query backend must also pass a conservative adversarial mix of document
  producers, covering the derive-once store's real per-content first-writer
  behavior. Cross-backend exact bytes and cosine are diagnostics. Semantic
  vector identity and versioned admission policy have separate digests, so a
  policy change recertifies backends without re-embedding unchanged vectors.
  The admitted registry remains empty until real placement and global evidence
  exists.
- **Local native-adapter boundary.** Target packages can expose Core ML,
  LiteRT, OpenVINO, or another native runner through the attested loopback
  embedding protocol that cfetch already consumes. The runner stays local and
  cfetch owns canonical INT8x768 output encoding; this is not remote inference.
- **Cross-major networking fails closed.** TCP, iroh ALPN, invites, grants, and
  remote memberships carry network major 1. Missing or different majors are
  rejected before slice data is served. `cfetch embedding-profile [--json]`
  prints the executable contract.
- **Public feature and benchmark guide.** The README now maps the product's
  agent-memory, retrieval, code-navigation, capture, measurement, serving, and
  sharing surfaces; includes the real v0.9.9 terminal dashboard; and compares
  cfetch with OpenWolf and OpenWolf Enhanced. A reproducible v0.9.9 study
  records 93.4% aggregate model-facing reduction across eight selected
  oversized command outputs, explicitly without claiming whole-session savings.
- **Organization-wide contributor terms.** External contributors retain
  copyright and grant the project broad copyright and patent rights through
  Corbet Labs' versioned Individual Contributor License Agreement. This keeps
  the whole project under one coherent FSL-to-Apache release promise.

## 0.9.9

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

- **Engine selection (historical 0.9.2 behavior; superseded on main by the
  candidate shared-output architecture above).** A variant is now a Cargo feature selection rather
  than a source fork: which engines are compiled in is a build-time fact,
  which device to use is a run-time one, and `cfetch hardware` reports both
  plus what it will actually do. A device no compiled-in engine can drive is
  skipped rather than selected and then failed on. At that release, everything
  fell back to the remote endpoint for a host that held nothing; main no longer
  permits automatic remote fallback.
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
