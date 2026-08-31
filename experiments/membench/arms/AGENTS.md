# AGENTS.md (agentsmd arm)

Hand-written project doc for the `agentsmd` arm — the "just write a good
README-style memory" baseline. Keep it task-relevant, never arm-specific:
the same file is used for every task in this arm.

## Conventions
- Scaled fields are normalized through helpers in src/normalize.js; never
  inline range checks.
- Money math must be exact: accumulate in integer cents or round per step.
- Tests are the source of truth for current behavior.

## Answers to common questions
- Validation rules live with the validators, not in handlers.
- Coupon percentages are PERCENT, applied consistently to net.
