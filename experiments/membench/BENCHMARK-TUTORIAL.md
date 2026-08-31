# Tutorial: Benchmarking openwolf-enhanced vs cfetch

Two questions, two harnesses, one afternoon of setup:

- **A. Quality & tokens** — does a memory tool make the agent better/cheaper? (`run-battery.ps1`)
- **B. Speed** — how fast is each tool on real repos? (`bench-speed.ps1`)

Run both on **one idle machine**, all arms sequentially. Record the environment
before you start — an unpublished benchmark is an opinion.

## 0. Pin and record everything

```powershell
# write this down once, keep it with the results
claude --version          # do NOT update mid-battery
cfetch --version          # note the commit: we benchmarked 0.9.9 @ 066a37b (post security fixes)
npx openwolf-enhanced --version   # use >= 1.28.0 (the hardened line)
node --version ; git --version
# machine: CPU, RAM, disk type, OS — wall numbers are only comparable to themselves
```

## 1. Test projects

Three real repos (mixed languages on purpose: cfetch parses Rust/JS/TS/Python/Go
with tree-sitter; openwolf treats everything as text — language coverage is part
of what you're measuring) plus the synthetic scale repo:

| repo | profile | why |
|---|---|---|
| `lodash/lodash` | small, pure JS (~0.5k files) | warm-up, fast iteration |
| `fastify/fastify` | medium, TS (~1k files) | realistic app shape |
| `tokio-rs/tokio` | medium-large, Rust (~2k files) | Rust grammar + deep module graph |
| `gen-scale-repo.mjs 300` | synthetic, 300 modules | controlled scaling without repo quirks |

```powershell
mkdir C:\repos ; cd C:\repos
git clone --depth 1 https://github.com/lodash/lodash
git clone --depth 1 https://github.com/fastify/fastify
git clone --depth 1 https://github.com/tokio-rs/tokio
# synthetic: node <this directory>\tasks\T05-scale\gen-scale-repo.mjs 300 C:\repos\scale300
```

Query terms per repo (things that actually exist in them):

- lodash: `chunk`, `debounce`, `isEqual`
- fastify: `router`, `schema`, `plugin`
- tokio: `spawn`, `runtime`, `select`
- scale300: `audit`, `cache`

## 2. Part A — quality & token battery

The four-task battery (T01 continuity, T02 locate, T03 bugfix, T04 staleness)
runs each arm — baseline, agentsmd, openwolf, cfetch — through the same agent
and counts tokens at a source that is not either tool.

```powershell
# terminal 1: the proxy (API-key mode)
cd <this directory>\proxy ; $env:ANTHROPIC_API_KEY="sk-..." ; .\run-proxy.ps1
#   ...or subscription mode: skip the proxy entirely and pass -UsageSource claude below

# terminal 2: the battery (dry-run first, always)
cd <this directory>\runner
.\run-battery.ps1 -DryRun
.\run-battery.ps1 -Repeats 3                    # ~96 sessions, 4-6 h sequential

# results
python ..\analysis\analyze.py                   # -> ..\results\report.md
```

**What each task tells you:**

| task | openwolf should win if… | cfetch should win if… |
|---|---|---|
| T01 continuity | its session memory re-injects the convention cheaply | ring-scoped resident memory carries it with fewer tokens |
| T02 locate | anatomy descriptions answer without opening files | code index + dependency graph answers with citations |
| T03 bugfix | — (both should match baseline; this is the correctness gate) | — |
| T04 staleness | memory.md updates fast when contradicted | quarantine+rings structurally resist the poisoned fact |

Reading the report: trust `tokens` medians only with n≥3 per cell; treat any
token win that costs `tasks ok` as a loss; `poisoned` is cfetch's home game.

## 3. Part B — speed benchmark

Per arm × repo, ~2–5 minutes each; run after Part A (or on a day you don't
touch the machine — wall numbers only):

```powershell
cd <this directory>\runner

# cfetch arm
.\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\lodash -Terms chunk,debounce,isEqual
.\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\fastify -Terms router,schema,plugin
.\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\tokio -Terms spawn,runtime,select
.\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\scale300 -Terms audit,cache

# openwolf arm (identical repos, identical terms)
.\bench-speed.ps1 -Arm openwolf -RepoSrc C:\repos\lodash -Terms chunk,debounce,isEqual
# ... same for fastify, tokio, scale300

# memory-search variant (both tools' recall path) on one repo for a taste:
.\bench-speed.ps1 -Arm cfetch   -RepoSrc C:\repos\fastify -Terms router -Mode recall
.\bench-speed.ps1 -Arm openwolf -RepoSrc C:\repos\fastify -Terms router -Mode recall

Import-Csv ..\results\speed.csv | Format-Table -AutoSize
```

**Metrics defined** (all land in `results\speed.csv`):

| metric | meaning | fairness note |
|---|---|---|
| `first_scan` | cold index of the repo | cfetch: `cfetch scan`; openwolf: `init + scan`. Different work — record, don't equate |
| `cached_scan` | second run, nothing changed | the steady-state cost you pay constantly; **this is the headline speed number** |
| `find_p50/p95_<term>` | code-search latency, 20 runs | process startup included — that's what the agent experiences |
| `recall_p50/p95` | memory-search latency | optional variant |
| `state_size` | MB the tool wrote outside the repo | disk cost of memory |

Validation run on this machine (cfetch, 30-module synthetic repo):
`first_scan 1.77s → cached_scan 0.05s, find p50 ~30ms, state 0.2MB`.
Expect openwolf's per-query cost to be dominated by Node startup
(hundreds of ms), cfetch's by SQLite — that difference is real and belongs
in the result, it is what an agent pays per lookup.

## 4. Reading Part B honestly

- **Don't compare `first_scan` across arms as "quality"** — they build
  different things (cfetch: symbol graph + FTS; openwolf: descriptions).
  Compare it only as "cost of adoption".
- **cached_scan and p50/p95 are the comparable core.**
- If a repo language isn't parsed by cfetch's grammars (anything beyond
  Rust/JS/TS/Python/Go), say so in the write-up — its `find` falls back to
  text and the comparison becomes text-vs-text.
- openwolf numbers via `npx` include npm resolution; install it globally
  first (`npm i -g openwolf-enhanced`) and edit the `$cmdFind` line to call
  the bare binary for its best case.

## 5. The mistakes checklist

1. Machine touched during runs → wall garbage (token/verdict metrics survive).
2. Tool versions unpinned or updated mid-battery.
3. Comparing first_scan as quality instead of cost.
4. n>1 run with n=1 confidence — no p-values below n=5 per cell.
5. Forgetting `-DryRun` before the first real battery and burning tokens on
   a broken harness.
6. Mixing `-UsageSource proxy` and `-UsageSource claude` in one dataset.
7. Running arms in parallel "to save time" — token attribution is
   time-window based and will silently misattribute.

## 6. One complete run day

```text
09:00  pin versions, clone repos, node _gen.mjs, .\run-battery.ps1 -DryRun
09:30  start proxy; run-battery -Repeats 3 (hands off the machine)
15:00  analyze.py; eyeball report.md; rerun any failed cells once
16:00  bench-speed both arms x 4 repos
17:00  copy results\*.csv + report.md + the version pin block into an archive
```
