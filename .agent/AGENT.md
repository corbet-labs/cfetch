# cfetch — agent knowledge

- The governing documents live in the operator's private knowledge tree under
  `todo/active/cfetch/`: **PRD.md** (product requirements — wins on conflict),
  **VOCAB.md** (canonical terms — use these words, not synonyms), DESIGN.md
  (mechanism dossiers + decision history) and STATUS.md (current state).
  Read PRD + VOCAB before implementing anything.
- ARCHITECTURE IN ONE PARAGRAPH: the markdown tree is the only storage of
  record. Per slice, a host holds STORAGE (the markdown), METADATA (derived
  artifacts) or NOTHING (remote queries only) — there is no cache tier. A
  serving daemon at each storage host watches its own files and answers every
  query behind a DRAIN BARRIER (serve-fresh-or-wait, bounded, always labeled
  {origin, generation, fresh}); seconds-scale drift between cooperating
  agents is a banned defect class, never fixed with a TTL. Derived work is
  computed ONCE at the change (scored across capable hosts) and distributed
  as content-addressed artifacts; nothing is derived twice.
- INDEXES ARE NEVER A FACT OF RECORD. Deleting any index must lose nothing.
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
