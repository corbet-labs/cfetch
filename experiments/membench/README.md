# membench — memory-tool benchmark harness

A reproducible benchmark for memory/context tools under a coding agent, built
to compare cfetch against openwolf-enhanced (and against two baselines: a
bare agent, and a handwritten AGENTS.md). Two parts:

- **Part A — quality & tokens** (`runner/run-battery.ps1`): four fixed tasks
  (convention continuity across sessions, code locate, bugfix with a
  regression guard, and a stale-memory poisoning probe), four arms, token
  counting from a LiteLLM proxy or from the agent's own usage block — never
  from the tools being measured.
- **Part B — speed** (`runner/bench-speed.ps1`): first scan, cached re-scan,
  query p50/p95, and state footprint on real repo clones.

Task seeds are generated: run `node tasks/_gen.mjs` once. Full methodology,
fairness rules, and per-task interpretation: see `BENCHMARK-TUTORIAL.md`.

## Recorded run — 2026-08-31, Windows

Environment: AMD Ryzen 9 5900X, 64 GB RAM, Windows 11, PowerShell 5.1,
Node 24.14.0. Arms: cfetch 0.9.9 (main @ 424efbf, includes the symbol_uses
fix this benchmark motivated — PR #21) vs openwolf-enhanced 1.28.1 (global
install, called as its bare binary). Repos: shallow clones of lodash,
fastify, tokio, plus the 300-module synthetic. Raw rows:
`results/speed-2026-08-31-windows.csv`.

| metric | cfetch | openwolf-enhanced | ratio |
|---|---|---|---|
| first scan (adoption cost) | 0.4–1.0 s | 1.2–3.3 s | ~2–3x |
| cached re-scan (steady state) | 0.04–0.06 s | 0.3–1.0 s | ~6–20x |
| query p50 | 31–35 ms | 105–119 ms | ~3.3x |
| query p95 | 33–45 ms | 110–125 ms | ~3x |
| state footprint | 0.3–5.6 MB | 0.2–0.9 MB | openwolf leaner; cfetch's tokio figure is the symbol/import graph |

Reading it honestly: the query gap is architecture (static Rust binary +
SQLite vs per-invocation Node process startup) and is what an agent pays per
lookup. The disk inversion is equally real — cfetch stores structure
(symbols, use sites, import edges), openwolf stores descriptions. Neither
number measures answer quality; that is Part A, which needs an agent and an
API and is one command in the tutorial.

The speed harness found its first bug within minutes: cfetch aborted on 7 of
48 lodash JS files (two same-named calls on one line produce a duplicate
`symbol_uses` primary-key tuple). Diagnosed from this harness, fixed in
PR #21 with a regression test built from the minimized trigger.

## Notes

- Runner and verifiers are PowerShell (5.1-compatible); on Linux run them
  under `pwsh` or port `verify.ps1` per task — the task files and the
  methodology are platform-neutral.
- `work/`, generated seeds, and fresh run outputs are ignored by the local
  `.gitignore`; only recorded results are committed, dated and
  environment-stamped.
