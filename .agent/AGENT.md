# cfetch — agent knowledge

- The design spec, the 7-ring model, and the mechanism dossiers live in the
  operator's private knowledge tree under `todo/active/cfetch/` (STATUS.md +
  DESIGN.md). Read them before implementing anything.
- CLEAN-ROOM RULE (load-bearing): this project implements mechanisms described in
  the private dossiers. Never port, translate, or paraphrase source code from
  `cytostack/openwolf` or `bassprofressor-lab/openwolf-enhanced` (both AGPL) —
  that is what keeps cfetch's own license possible.
- All output, comments, and docs in English.
- Commit directly to `main`, no AI attribution lines.
- PUBLIC GENERAL TOOL (load-bearing): this repo is public and cfetch is a
  general product, not operator-specific tooling. Never commit private
  infrastructure details — no LAN/overlay IPs, hostnames, usernames, service
  names, or paths from the operator's network, not even in tests, fixtures,
  comments, or commit messages. Use RFC 5737/3849 documentation addresses and
  generic slugs in fixtures. Defaults must work for any deployment; anything
  operator-specific belongs in the operator's config, never in code.
