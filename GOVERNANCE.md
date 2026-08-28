# Governance

cfetch is maintained by the Corbet Labs Maintainers team. Julian Corbet is the
project lead and final decision-maker.

## Decisions

- Small implementation decisions are resolved in issues and pull requests.
- Changes to architecture, trust boundaries, storage semantics, licensing, or
  compatibility require explicit maintainer approval and durable documentation.
- Measurements belong in `studies/`; exploratory code belongs in `experiments/`.
- Security reports follow the private process in `SECURITY.md`.

The maintainer may decline work that conflicts with the product scope, increases
maintenance cost without sufficient value, weakens a trust boundary, or lacks
evidence for a load-bearing design change.

## Contributions and releases

Contributors retain copyright and agree to Corbet Labs' organization-wide
[Individual Contributor License Agreement](https://github.com/corbet-labs/.github/blob/cla-v1.0/CLA.md).
Every merge must pass the required CI and license gates. Releases are tagged
from verified commits and follow the process documented in the repository
workflows and changelog.

cfetch remains on the 0.x release line. Version 1.0 and every later major are
blocked until the project lead explicitly changes that policy.

## Changes to governance

Governance changes use the same public review path as other project changes.
The current version on `main` is authoritative.
