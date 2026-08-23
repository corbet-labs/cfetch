# cfetch v0.9.9 context-reduction benchmark

This study measures one bounded claim: how much model-facing context cfetch's
PostToolUse output governor removes from oversized command results. It does not
measure total session cost, task success, answer quality, or the value of memory
retrieval.

## Result

| Measurement | Result |
|---|---:|
| Command outputs | 13 |
| Oversized outputs rewritten | 8 |
| Original size of rewritten outputs | 86,469 estimated tokens |
| Model-facing replacement size | 5,702 estimated tokens |
| Estimated tokens avoided | 80,767 |
| Aggregate reduction | 93.4% |
| Median per-output reduction | 91.2% |
| Short outputs passed through unchanged | 4 of 4 |
| Long verification output passed through unchanged | 1 of 1 |

The replacement size includes cfetch's pointer to the preserved full output.
All token figures use the binary's labeled `ceil(characters / 3.5)` estimate.
The original character count and emitted character count are recorded together
at the hook rewrite point; the token estimate is derived from those two observed
values.

## Inputs

The benchmark used the released Linux x86-64 `cfetch 0.9.9` binary at commit
`6c525228d9e7157a3490d6e1f18e20aaee1d5aa4`.

Repository inputs were pinned to:

- `corbet-labs/cfetch` at `e3669344893ff7c56dcec2a8135158042da50a68`
- `corbet-labs/dotkeeper` at `0a3c890742321533fbf7a939c715a83839d805a7`

Each command's stdout was passed through the real Codex-shaped `post-tool` hook
entrypoint. No benchmark-only condensation path was used.

| Case | Output lines | Original tokens | Entered tokens | Reduction |
|---|---:|---:|---:|---:|
| cfetch public API/code-shape search | 586 | 12,068 | 999 | 91.7% |
| cfetch history with changed paths | 689 | 6,056 | 437 | 92.8% |
| cfetch history with diff statistics | 541 | 7,806 | 754 | 90.3% |
| cfetch repository tree | 88 | 463 | 364 | 21.4% |
| dotkeeper public API/code-shape search | 1,453 | 41,810 | 1,400 | 96.7% |
| dotkeeper history with changed paths | 769 | 7,770 | 542 | 93.0% |
| dotkeeper history with diff statistics | 551 | 8,664 | 809 | 90.7% |
| dotkeeper repository tree | 213 | 1,832 | 397 | 78.3% |

The repository-tree case shows why both sides are retained: cfetch's internal
condensation threshold applies before the preservation pointer is added, while
the reported 21.4% is the smaller, honest reduction after including that pointer.

The four short controls ranged from one to four lines and were emitted
unchanged. The verification control was the successful Linux job log from
GitHub Actions run `32628213926`: 2,150 lines and 254,310 characters presented
to the hook as `cargo test --all`. It was also emitted unchanged. cfetch treats
test and build output as non-rewritable because a failure can occur anywhere in
the log.

## Commands

The oversized cases used these command shapes against both repositories where
applicable:

```console
git log --stat --oneline -100 origin/main
git log --name-status --oneline -150 origin/main
git grep -n -E '<language-specific public API pattern>' origin/main -- <source paths>
git ls-tree -r --name-only origin/main
```

The short controls used `git show --stat --oneline origin/main` and a focused
`TODO|FIXME|SAFETY|NOTE` search.

## What can be claimed

This run supports the statement that cfetch reduced eight selected oversized
repository outputs by 93.4% in aggregate, including its preservation pointers,
while leaving all short and verification controls untouched.

It does not support a claim that cfetch reduces every command, every session,
or total model usage by 93.4%. Whole-session claims require paired agent runs.
`cfetch bench` enforces that distinction by pairing transcripts on an identical
first prompt and refusing a headline delta with fewer than three pairs.
