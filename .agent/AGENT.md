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
