# Contributing to cfetch

Contributions are welcome. Three things to know before your first pull request:

## 1. License and contributor agreement

The project is licensed under [FSL-1.1-ALv2](LICENSE.md) (each release becomes
Apache-2.0 after two years). External contributors retain copyright in their
work and grant the rights described in Corbet Labs' organization-wide
[Individual Contributor License Agreement](https://github.com/corbet-labs/.github/blob/cla-v1.0/CLA.md).

Signing is automated: on your first pull request, the contributor-agreement bot
asks you to post this exact comment:

> I have read and agree to version 1.0 of the Individual Contributor License Agreement at https://github.com/corbet-labs/.github/blob/cla-v1.0/CLA.md.

The bot records the signature for future contributions to this repository.
Forking the repository to prepare a pull request is expected and welcome.

## 2. Clean-room rule

cfetch reimplements *mechanisms* studied in the AGPL projects
`cytostack/openwolf` and `bassprofressor-lab/openwolf-enhanced`, without using
their code. Do not copy, translate, or closely paraphrase source from either
project (or any other incompatibly licensed work) into a contribution. The
contributor agreement requires you to own the rights you grant and to follow
these third-party-material instructions. Behavior, ideas, and measurements are
fair game; their expression is not.

## 3. Third-party provenance

Prefer adding a dependency or contributing a fix upstream over copying code
into cfetch. If a contribution does include third-party material, the pull
request must identify, for every copied part:

- the upstream project URL and exact revision;
- the original file path and the destination path in cfetch;
- the applicable license and copyright holder;
- any required NOTICE text; and
- what was modified after copying.

This record is how the contribution satisfies section 3(d) of the contributor
agreement. Copied material remains under its upstream license and is not treated
as the contributor's original work. Missing provenance is a merge blocker.

## 4. House rules

- Follow the [Code of Conduct](CODE_OF_CONDUCT.md).
- Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md),
  never in a public issue.
- All code, comments, commit messages, and docs in English.
- `experiments/` is for throwaway spikes, `studies/` for written-up measurements
  that justify design decisions; shipped code lives in `src/` with tests.
- Small, reviewable PRs against `main`.
