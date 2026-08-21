# cfetch

A second brain for coding agents: kernel-style privilege rings over one global
knowledge tree — unconditional hook injection, ranked retrieval with cited
memories, and a code index, in one Rust binary.

Your agent's accumulated knowledge (rules, decisions, facts, working state)
lives as plain markdown in a single git-tracked tree (by default `~/agents`).
cfetch assigns every statement a **privilege ring**, kernel-style — ring 0 is
the highest privilege, and when statements contradict, the lower ring wins:

| Ring | Name | Contents | Reaches the agent |
|------|------|----------|-------------------|
| 0 | Invariants | Hard guards, never-do rules | always injected |
| 1 | Policy | Locked-in decisions, standing authorities | always injected |
| 2 | Behavior | Distilled feedback: how to work | injected where it applies |
| 3 | Knowledge | Curated facts: hosts, projects, world | on demand (`recall`) |
| 4 | State | Todo queues, working notes | on demand (`recall`) |
| 5 | Staging | Promotion candidates, quarantined | never implicitly |
| 6 | Exhaust | Raw capture from sessions | never implicitly |

A file's ring defaults by location — the mapping is yours, see
[Configuration](#configuration) — and can be overridden per file with
`ring: N` frontmatter. Rings 0–1 are injected at session start through Claude
Code hooks; a ring-2 file is injected too, but only into the sessions it is
scoped to (a host, a repo). Rings 0–4 are searchable; every hit carries a
ring-prefixed, content-addressed citation, so the id itself reveals how much to
trust the statement. Rings 5–6 (automatic capture and its staging area) never
reach an agent's context implicitly — captured exhaust is untrusted input by
definition, and both live as plain text in the tree so any host can review
them. Automatic capture, the promotion traps and the staging queue are live,
as are recall, the code index, scoped injection, MCP and the dashboard.

## One brain, many machines

cfetch does not replicate your knowledge across machines and hope the copies
agree. The machine that **holds** the markdown runs a serving daemon; every
other machine holds **nothing** and asks it:

```console
# on the machine with the files
$ cfetch daemon start        # serve.enabled in config: watches the tree, answers queries

# on any other machine (config: client.serving = { addr, token_file })
$ cfetch recall zfs backup
r3-b7e2519cc0 knowledge/hosts/backups.md:12-19 (ring 3)
    The mirror job snapshots hourly and prunes by tier ...

served by workstation (generation 412, fresh)
```

Two properties make that safe to rely on:

- **Serve-fresh-or-wait.** Every query passes a drain barrier: the answer is
  computed only after every write the daemon could already have seen has been
  indexed. An agent reads its teammate's write, not the world before it.
- **Freshness is never assumed.** Every answer carries its origin, catalog
  generation, and a `fresh` flag. If the barrier expires the answer still
  comes — labeled stale, with the reason. Silent staleness is the one failure
  this design refuses.

How the barrier proves coverage depends on what the platform's file watcher can
promise, and `cfetch status` says which is in force. On Linux, inotify delivers
events in order, so a numbered sentinel riding the same queue proves it for
free. Where events carry no usable order — macOS FSEvents, kqueue, Windows,
polling — the barrier proves coverage by content instead, comparing a stat
fingerprint of the tree taken at query entry against what the committed catalog
has seen. That costs a stat walk per query (tens of milliseconds on a normal
brain) and it is not optional: a guarantee the platform cannot give is not a
guarantee, and an answer that cannot be proven fresh says so.

A machine that holds nothing opens no database, keeps no index, and needs no
storage: it is a network call away from the whole brain.

## What you get

- **`cfetch recall`** — BM25-ranked search over rings 0–4, ring-prefixed
  citations, 1-hop wikilink expansion, `--json` for scripts.
- **`cfetch find`** — a tree-sitter code index over your project roots
  (Rust, TypeScript/JavaScript, Python, Go): symbols with exact line ranges
  and token estimates, so agents read the 40 relevant lines instead of the
  whole file.
- **Hooks** — `cfetch install` registers session-start injection in Claude
  Code. Hooks are thin clients of a warm per-host daemon and always exit 0;
  a broken brain degrades to silence, never to a broken session.
- **MCP** — `cfetch mcp` serves `cfetch_recall` / `cfetch_expand` /
  `cfetch_find` over stdio to any MCP client (Claude Desktop, Codex, Gemini
  CLI).
- **`cfetch dashboard`** — a terminal dashboard: daemon and hook health,
  injection ledger, live recall.
- **`cfetch selfcheck` / `cfetch status`** — verify the installation end to
  end; surface silently-failing hooks instead of letting the brain die
  unnoticed for weeks.

## Install

Prebuilt binaries are attached to [GitHub releases](https://github.com/julian-corbet/cfetch/releases).
From source:

```console
# cargo (any platform with Rust 1.85+)
cargo install --git https://github.com/julian-corbet/cfetch

# nix (flake; x86_64-linux and aarch64-linux)
nix profile install github:julian-corbet/cfetch

# Arch Linux
git clone https://github.com/julian-corbet/cfetch
cd cfetch/packaging/arch && makepkg -si
```

### Platforms

Linux, macOS and Windows are all first-class. One binary, one behaviour; the
platform differences are confined to three places:

| | Linux / macOS | Windows |
|---|---|---|
| Daemon control channel | unix socket (`$XDG_RUNTIME_DIR/cfetch.sock`, else the state dir) | loopback TCP on an ephemeral port, gated by a per-daemon token, both published in `daemon.endpoint` in the state dir |
| State dir | `~/.local/state/cfetch` | `%LOCALAPPDATA%\cfetch` |
| Config | `~/.config/cfetch/config.json` | `%APPDATA%\cfetch\config.json` |

`CFETCH_STATE_DIR`, `CFETCH_CONFIG`, `CFETCH_BRAIN` and `HOME` override the
defaults identically on every platform. A unix socket file is access-
controlled by its mode; a loopback TCP port is not, which is why the Windows
control channel carries a bearer token — the same gate the optional serving
listener (`serve.bind`) uses.

## Quick start

```console
$ cfetch install          # register hooks in ~/.claude/settings.json (+ Codex/Gemini MCP if present)
$ cfetch daemon start     # warm per-host daemon (optional but recommended)
$ cfetch scan             # build the recall + code index
indexed 412 docs, 3187 blocks (2 file(s) skipped as ring 5+)
code: 66 files, 891 symbols (re)parsed
```

Search the brain — lower ring means higher trust:

```console
$ cfetch recall zfs backup
r1-3f9c04a2d8 AGENT.md:101-104 (ring 1)
    Native filesystem backup only — never file-level tools between ZFS datasets ...
r3-b7e2519cc0 knowledge/hosts/server/backups.md:12-19 (ring 3)
    The mirror job snapshots hourly and prunes by tier; restores are tested by ...

expand a hit: cfetch recall --id <citation>
```

Locate code with exact ranges:

```console
$ cfetch find segment
src/index.rs:149-171  function_item segment  (~180 tok)
src/index.rs:723-741  function_item segment_skips_frontmatter  (~150 tok)
```

For Claude Desktop (or any other MCP client), register the binary as a stdio
server:

```json
{ "mcpServers": { "cfetch": { "command": "cfetch", "args": ["mcp"] } } }
```

`cfetch install` writes the equivalent entries for Codex (`~/.codex/config.toml`)
and Gemini CLI (`~/.gemini/settings.json`) automatically when those are
installed, using marker-fenced blocks and tagged entries — your own settings
are never touched, and `cfetch install --remove` takes exactly ours back out.

## Configuration

`~/.config/cfetch/config.json` (all keys optional; partial files merge over
defaults):

```json
{
  "brain_root": "/home/you/notes",
  "resident": [
    { "path": "AGENT.md", "ring": 1 },
    { "path": "rules/invariants.md", "ring": 0 }
  ],
  "code_roots": ["projects"],
  "budget_chars": 6000,
  "exhaust_max_bytes": 33554432,
  "ledger_max_bytes": 8388608
}
```

- `brain_root` — the knowledge tree (default `~/agents`, or `$CFETCH_BRAIN`).
- `resident` — files injected verbatim (budget-clipped) at session start, in
  order. Rings 0–1 may be injected anywhere; ring 2 only with a `scope` (see
  below); rings 3+ are refused at load time. An explicitly empty list means
  "inject nothing" — useful where the harness already auto-loads these files,
  so they are not paid for twice.
- `code_roots` — roots for the code index, relative to `brain_root` unless
  absolute. Empty means `<brain_root>/projects/github`. A running daemon keeps
  this index current on its own — an initial scan once its file watches are up,
  then an incremental refresh on the same 60-second cadence as the integrity
  backstop — so `cfetch find` and `cfetch map` answer on a freshly started host
  without anyone running `cfetch scan` by hand.
- `budget_chars` — hard cap on the injected digest.
- `exhaust_max_bytes` — writer-side cap on this host's ring-6 exhaust stream
  before it rotates; two rotated generations are kept.
- `ledger_max_bytes` — the same, for this host's ledger stream.

### Ring assignment (`ring_rules`)

Which ring a file lands on is a property of your tree, so it is configuration.
`ring_rules` is an ORDERED list; the **first matching rule wins**, so a
specific rule goes above a general one and nothing depends on counting prefix
characters. A prefix ending in `/` matches the whole subtree; a prefix without
a trailing slash matches that exact path only (`AGENT.md` never captures
`AGENT.md.bak`); the empty prefix `""` matches everything and is how a list
declares its own catch-all. A path no rule matches lands on **ring 3**. A
`ring: N` key in a file's frontmatter still overrides whatever the rules say.

The shipped default, which you replace wholesale by setting the key:

```json
{
  "ring_rules": [
    { "prefix": "AGENT.md", "ring": 1 },
    { "prefix": "README.md", "ring": 1 },
    { "prefix": "mind/memories/MEMORY.md", "ring": 1 },
    { "prefix": "mind/memories/", "ring": 2 },
    { "prefix": "todo/", "ring": 4 },
    { "prefix": "staging/", "ring": 5 }
  ]
}
```

A tree organized differently just says so — for example:

```json
{
  "ring_rules": [
    { "prefix": "rules/invariants.md", "ring": 0 },
    { "prefix": "handbook/", "ring": 1 },
    { "prefix": "habits/", "ring": 2 },
    { "prefix": "tasks/", "ring": 4 },
    { "prefix": "", "ring": 3 }
  ]
}
```

Rings run 0–6; a rule naming anything higher is refused at load.

`staging/` is shipped as ring 5 because the LOCATION decides: a staged
candidate whose frontmatter is stripped or hand-mangled is still never
recallable. Point the rule elsewhere if your staging area lives elsewhere, but
do not delete it — ring 5 is the ladder's quarantine.

### Exclusions (`exclude_prefixes`)

Two layers, deliberately:

- **Hard boundary, not configurable.** The secrets directory
  (`mind/secrets/`), logs (`logs/`), and git internals (any `.git/`, at any
  depth) never enter the index and are never watched — no config can lift
  that. Secret-shaped filenames (`.env*`, `*.key`, `*.pem`, `*credential*`,
  `*password*`, `*secret*`) are refused wherever they live, and secret-shaped
  paths are withheld from session capture as well.
- **Your own exclusions.** `exclude_prefixes` adds to that boundary. The
  shipped value is `["projects/", "knowledge/archive/"]` — repo clones belong
  to the code index rather than the prose index, and an archive is retired
  knowledge you should not recall by accident. Both are conventions, so both
  are yours to change:

```json
{ "exclude_prefixes": ["vendor/", "attachments/", "scratch/"] }
```

A prefix matches on path components, so `drafts` excludes `drafts/note.md`
but never `draftsman.md`; a trailing slash is optional.

### Scoped injection

Injection is policy, not a fixed resident set: with many domains there is no
universal most-important file. Any resident entry may carry a `scope`, and
only sessions it matches receive it.

```json
{
  "resident": [
    { "path": "AGENT.md", "ring": 1 },
    { "path": "rules/build-host.md", "ring": 1,
      "scope": { "hosts": ["build-box"] } },
    { "path": "habits/widget-review.md", "ring": 2,
      "scope": { "repos": ["widget", "widget-docs"] } },
    { "path": "rules/invariants.md", "ring": 0,
      "scope": { "always": true } }
  ]
}
```

- `hosts` — machine names. Matched against this machine's hostname, either in
  full or by its first label (`build-box` also matches
  `build-box.example.net`).
- `repos` — the name of the directory the session was started in (the last
  path component of the agent's working directory).
- `always` — inject regardless of host and repo. An entry with no `scope` at
  all already means everywhere; `always` states it explicitly so that adding
  one host to the list later cannot narrow it by accident.
- `hosts` and `repos` are ORed: an entry listing both arrives on any listed
  host AND in any listed repo, not only where the two coincide.

Ring 2 (distilled behavior) is injectable only WITH a scope — that is what
makes it selective rather than a second unconditional set. `cfetch selfcheck`
prints which entries the current host and directory left out, so a file that
stops arriving is explainable without reading the config.

### Semantic recall (`embeddings`)

Off by default. Point it at any OpenAI-compatible `/embeddings` endpoint — a
local llama.cpp server, LM Studio, vLLM, or a hosted API:

```json
{
  "embeddings": {
    "enabled": true,
    "endpoint": "http://127.0.0.1:8080/v1",
    "model": "embed-model",
    "dimensions": 1024,
    "precision": "f16",
    "api_key_env": "MY_EMBED_KEY"
  }
}
```

- `dimensions` — the vector width to ask for and to store (default 1024). It
  is sent to the endpoint as `dimensions`, which Matryoshka-trained embedders
  honor; an endpoint that ignores it gets its vector truncated to that prefix
  and re-normalized client-side. A model that cannot reach the width is an
  error, never a silently narrower vector. Full native width is usually 8–16x
  more than a documentation corpus can use, and it dominates the index file.
- `precision` — `f16` (default) or `f32`. Half floats halve the artifact at a
  cosine error far below the ranking's resolution.
- `api_key_env` — the NAME of an environment variable holding the key, never
  the key. Endpoints are SSRF-guarded: https or loopback only, private and
  metadata ranges refused unless listed in `allow_hosts`.

Then derive the vectors once:

```console
$ cfetch embed-index
embedded 3187/3187 blocks
embed-index complete: 3187 embedded this run, 0 imported from the shared store, 3187 block(s) total
```

Vectors are keyed by the CONTENT HASH of the block — the same digest the
citation shows a prefix of. Two consequences:

- Editing a file costs only the blocks that changed. A rescan keeps every
  unchanged block's vector and prunes only hashes that left the tree.
- They are a property of the content, not of a machine. They are written to
  `<brain_root>/state/cfetch/vectors/` as one packed artifact file plus a
  hash index per `(model, dimensions, precision)`. Any host that can reach
  the tree READS them; only a host with an endpoint configured writes.
  `embed-index` derives only hashes no host has derived yet, and resumes
  where it stopped. The per-host database keeps a cache of the same vectors —
  never the record. The shared store is append-only: a vector whose text left
  YOUR tree may still be someone else's slice, so it is kept, and the artifact
  count is printed next to the block coverage rather than quietly diverging
  from it.

`--semantic` ranks by cosine alone, `--hybrid` fuses it with BM25 (reciprocal
rank fusion, `recall.rrf_k`). Neither degrades quietly: when vector coverage
is partial or zero, the numbers are reported on stderr (and as
`semantic_note` in `--json`) and the answer still comes back lexically.

```console
$ cfetch recall --hybrid zfs backup
cfetch recall: semantic: 0/3187 blocks embedded — answering lexically only; run cfetch embed-index
r1-3f9c04a2d8 AGENT.md:101-104 (ring 1)
    Native filesystem backup only — never file-level tools between ZFS datasets ...
```

`cfetch status` prints the same coverage before you need it.

### Reranking (`rerank`)

Off by default, and independent of how the candidates were found — it improves
a lexical, semantic or hybrid list alike.

```json
{
  "rerank": {
    "enabled": true,
    "endpoint": "http://127.0.0.1:8080/v1",
    "model": "bge-reranker-v2-m3",
    "candidates": 40
  }
}
```

Retrieval scores a query against a document it never sees beside it: BM25
counts terms, and a bi-encoder compares two vectors that were built
independently. A cross-encoder reads query and document TOGETHER and judges
relevance far better — but it costs one forward pass per candidate, so it can
only run over a shortlist. Recall proposes, rerank reorders.

- `candidates` — how many hits are retrieved and sent to the cross-encoder.
  Recall widens to this number and the answer is cut back to your `--limit`
  afterwards, because a reranker can only promote what retrieval proposed.
  Everything past the window keeps its retrieval order and follows.
- Scores are whatever the model emits — cross-encoder logits are commonly
  negative and unbounded. Only their ORDER is used, never their magnitude, and
  equal scores keep retrieval order rather than shuffling.

Reranking never decides whether an answer comes back. An unreachable endpoint,
an unparseable response, or a score list that does not line up with what was
sent all return the retrieval order with the reason attached (on stderr, and
as `rerank_note` in `--json`).

```console
$ cfetch recall --hybrid zfs backup
cfetch recall: rerank unavailable (POST http://127.0.0.1:8080/v1/rerank: connection refused) — answering in retrieval order
r1-3f9c04a2d8 AGENT.md:101-104 (ring 1)
    Native filesystem backup only — never file-level tools between ZFS datasets ...
```

## What lives where

The tree is the only storage of record, and that includes the automatic half
of the ladder:

```
<brain_root>/logs/cfetch/exhaust-<host>.jsonl   ring-6 capture, append-only
<brain_root>/logs/cfetch/ledger-<host>.jsonl    injection + measured usage
<brain_root>/staging/cfetch/<id>.md             ring-5 candidates (ring: 5)
<brain_root>/staging/cfetch/dismissed/<id>.md   candidates ruled out, kept
```

Both stream formats are versioned line by line (`{"v":1,…}`) and refused
rather than guessed at on a version this binary does not know. One file per
host means concurrent machines never interleave, and every reader — `cfetch
status`, `cfetch audit`, `cfetch staging list` — folds ALL of them, so a
candidate flagged on one machine is visible to a distillation session on
another.

Everything else cfetch keeps is DERIVED and rebuildable: the index database,
heartbeats and session state live per-host in `~/.local/state/cfetch`, never
inside the shared tree, which may be shared between machines over NFS. Vectors
are the one derived exception: they are a property of the CONTENT rather than
of a machine, so they live in `<brain_root>/state/cfetch/vectors/`
(self-ignoring for git) and every host that can reach the tree reads them.

## License

[FSL-1.1-ALv2](LICENSE.md) — the Functional Source License. You can use,
modify, and redistribute cfetch for any purpose except offering a competing
commercial product or service. Each release automatically becomes
[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0) two years after it
ships.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Two things
to know up front:

- Every contributor signs the [CLA](CLA.md) once, by posting a sentence on
  their first pull request (a bot walks you through it). You keep your
  copyright; the grant keeps the project relicensable as a single-owner work.
- cfetch is a clean-room implementation of mechanisms studied in two AGPL
  projects. Do not copy or closely paraphrase code from them (or any other
  incompatibly-licensed work) into a contribution.
