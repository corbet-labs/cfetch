# Supervised AI memory maintenance

cfetch can use the coding agent that is already connected over MCP or running
the CLI to analyze memory candidates. The model is an analyst and patch author,
not the authority that decides what becomes trusted memory.

## The lifecycle

1. Hooks append redacted activity to ring 6. Deterministic traps flag repeated
   failures, discovered fixes, and unusually hot trusted files as ring-5
   candidates.
2. `cfetch maintain packet <candidate-id>` builds a bounded evidence packet.
   Raw events, the candidate file, relevant current statements, and target
   snapshots receive content-addressed identifiers.
3. An agent returns a typed proposal. The CLI accepts JSON from a file or stdin;
   MCP exposes `cfetch_maintenance_propose`. Both routes write only under
   `todo/staging/maintenance/pending/`, which is ring 5 by location.
4. A separate agent pass or a human records the first immutable semantic review
   with `cfetch maintain review <proposal-id>`. It must explicitly assess
   evidence coverage, factual faithfulness, preservation, authority, target
   choice, and contradictions. A failed review requires a revised proposal and
   therefore a new content address; reviews cannot be replaced.
5. `cfetch maintain verify <proposal-id>` independently checks the proposal
   and review against current files and evidence. A passing report prints an
   approval token bound to both exact records.
6. `cfetch maintain apply ... --approval-token ...` re-runs every check and
   writes the exact proposed Markdown. The operation can be reversed with
   `cfetch maintain revert` while it remains unfinalized.
7. After normal review and a git commit, `cfetch maintain finalize` verifies
   that `HEAD` contains the exact approved bytes. Only then does cfetch consume
   the source candidate. Dismiss and no-op decisions preserve the candidate in
   the dismissed audit lane instead.

Proposal states are visible with `cfetch maintain list`. Proposal Markdown is
kept in the knowledge tree, while the folder name records whether it is pending,
applied, finalized, rejected, or reverted.

## Proposal JSON

```json
{
  "candidate_ids": ["recurring-failure-a1b2c3d4"],
  "transition": "fold",
  "target": "knowledge/build-troubleshooting.md",
  "after": "---\nring: 3\n---\n\n# Build troubleshooting\n\n...\n",
  "authority": "attested",
  "valid_until": null,
  "rationale": "Two captured sessions show the same failure and recovery.",
  "evidence": ["e6-3a9f812ce474df20"],
  "related_citations": ["r3-91c5ef12a0"]
}
```

Transitions have specific meanings:

| Transition | Meaning |
|---|---|
| `add` | Create a new curated Markdown file |
| `fold` | Merge evidence into an existing statement without duplicating it |
| `supersede` | Replace a stale or contradicted statement while preserving the reasoning in git history |
| `revalidate` | Refresh a statement whose validity was checked again |
| `dismiss` | Record that the candidate should not become memory |
| `noop` | Record that current memory already covers the candidate |

The `after` field is the complete target file, not a free-form patch. cfetch
captures the current file itself, displays a byte-exact diff, and rejects the
proposal if the file changes before apply. This avoids ambiguous patch context
and makes rollback exact.

The semantic review is also typed:

```json
{
  "verdict": "pass",
  "method": "independent_agent",
  "evidence_coverage": true,
  "factual_faithfulness": true,
  "preservation": true,
  "authority_fit": true,
  "target_fit": true,
  "contradiction_checked": true,
  "notes": "The proposed statement is fully supported and does not duplicate or contradict current memory."
}
```

The schema makes the reviewer state what it checked; it does not pretend to
prove that two model calls are independent. Use a separate agent pass or select
`human` as the review method. The immutable review and later explicit approval
keep that distinction visible in the audit trail.

## Authority is separate from trust

Rings describe how trusted and how readily served a statement is. Authority
describes who or what is allowed to support the claim.

| Authority | Meaning | Allowed maintenance result |
|---|---|---|
| `authorized` | Direct operator instruction | Ring 2 or ring 3 |
| `attested` | Independently observable evidence | Ring 3 |
| `unendorsed` | Third-party text or model inference | Dismiss/no-op only |

Maintenance cannot write rings 0 or 1. Those contain critical invariants and
settled policy and remain manually authored.

## Deterministic gates

A proposal cannot be applied unless all current checks pass:

- proposal and candidate content addresses match;
- the first immutable semantic review passed every named check against the
  exact proposal bytes;
- each candidate is covered by a matching raw event, or by its candidate record
  when no raw event remains;
- cited statements still resolve to the bytes the agent reviewed;
- the target is a brain-relative, symlink-free, indexable Markdown path;
- the effective frontmatter/path ring is 2 or 3 and authority permits it;
- the target still matches the captured before bytes;
- no other applied proposal owns the same target;
- optional validity has not expired;
- secret-shaped content is refused before the proposal enters the shared tree.

Finalization adds two more checks: the target must have no uncommitted changes,
and git `HEAD` must contain the proposal's exact after bytes.

## What cfetch deliberately does not do

- It does not schedule model calls or create an inference bill in the daemon.
- It does not let an MCP client apply trusted-memory changes.
- It does not summarize away the raw event before review.
- It does not treat model confidence as authorization.
- It does not consume evidence merely because a plausible proposal was written.

This keeps the Markdown tree and git history as the record, while derived
indexes and model choices remain replaceable.
