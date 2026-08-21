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
| 2 | Behavior | Distilled feedback: how to work | selective injection |
| 3 | Knowledge | Curated facts: hosts, projects, world | on demand (`recall`) |
| 4 | State | Todo queues, working notes | on demand (`recall`) |
| 5 | Staging | Promotion candidates, quarantined | never implicitly |
| 6 | Exhaust | Raw capture from sessions | never implicitly |

A file's ring defaults by location and can be overridden per file with
`ring: N` frontmatter. Rings 0–1 are the resident set, injected at session
start through Claude Code hooks. Rings 0–4 are searchable; every hit carries a
ring-prefixed, content-addressed citation, so the id itself reveals how much to
trust the statement. Rings 5–6 (automatic capture and its staging area) never
reach an agent's context implicitly — captured exhaust is untrusted input by
definition. The capture/staging pipeline is still in progress; recall, the code
index, injection, MCP, and the dashboard work today.

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
  "brain_root": "/home/you/agents",
  "resident": [
    { "path": "AGENT.md", "ring": 1 },
    { "path": "mind/invariants.md", "ring": 0 }
  ],
  "code_roots": ["projects/github"],
  "budget_chars": 6000,
  "ledger_max_sessions": 200
}
```

- `brain_root` — the knowledge tree (default `~/agents`, or `$CFETCH_BRAIN`).
- `resident` — files injected verbatim (budget-clipped) at session start, in
  order. Only rings 0–1 may be resident; anything else is refused at load
  time. An explicitly empty list means "inject nothing" — useful where the
  harness already auto-loads these files, so they are not paid for twice.
- `code_roots` — roots for the code index, relative to `brain_root` unless
  absolute. Empty means `<brain_root>/projects/github`.
- `budget_chars` — hard cap on the injected digest.
- `ledger_max_sessions` — sessions kept in the injection ledger.

Ring assignment: `AGENT.md` and the memory index default to ring 1, distilled
memories to ring 2, `todo/` to ring 4, everything else to ring 3; a
`ring: N` frontmatter key overrides. Secrets directories, secret-shaped
filenames (`.env`, `*.key`, `*credentials*`, ...), logs, and archives are
excluded from indexing as a hard boundary, not as configuration.

Mutable state (index database, ledger, heartbeats) lives per-host in
`~/.local/state/cfetch` — never inside the brain tree, which may be shared
between machines over NFS.

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
