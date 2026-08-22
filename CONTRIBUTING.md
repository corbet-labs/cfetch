# Contributing to cfetch

Contributions are welcome. Three things to know before your first pull request:

## 1. License and CLA

The project is licensed under [FSL-1.1-ALv2](LICENSE.md) (each release becomes
Apache-2.0 after two years). To keep the project relicensable as a single-owner
work, every contributor must sign the [Contributor License Agreement](CLA.md) —
a license grant (you keep your copyright), based on the Apache Individual CLA.

Signing is automated: on your first pull request, the CLA bot asks you to post

> I have read the CLA Document and I hereby sign the CLA

as a PR comment. One signature covers all your future contributions.

## 2. Clean-room rule

cfetch reimplements *mechanisms* studied in the AGPL projects
`cytostack/openwolf` and `bassprofressor-lab/openwolf-enhanced`, without using
their code. Do not copy, translate, or closely paraphrase source from either
project (or any other incompatibly-licensed work) into a contribution — the CLA
makes you warrant that you haven't. Behavior, ideas, and measurements are fair
game; their expression is not.

## 3. Third-party provenance

Prefer adding a dependency or contributing a fix upstream over copying code
into cfetch. If a contribution does include third-party material, the pull
request must identify, for every copied part:

- the upstream project URL and exact revision;
- the original file path and the destination path in cfetch;
- the applicable license and copyright holder;
- any required NOTICE text; and
- what was modified after copying.

This record is how the contribution satisfies section 5(c) of the CLA. Copied
material remains under its upstream license and is not treated as the
contributor's original work. Missing provenance is a merge blocker.

## 4. House rules

- All code, comments, commit messages, and docs in English.
- `experiments/` is for throwaway spikes, `studies/` for written-up measurements
  that justify design decisions; shipped code lives in `src/` with tests.
- Small, reviewable PRs against `main`.
