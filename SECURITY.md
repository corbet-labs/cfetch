# Security Policy

## Supported versions

cfetch is pre-1.0 and moves quickly. Security fixes are made only on the latest
published 0.x release line.

| Version | Supported |
|---|---|
| Latest published 0.x | Yes |
| Older 0.x releases | No |
| 1.0 and later | Not published; blocked by project policy |

Upgrade to the latest release before reporting a problem that may already be
fixed.

## Reporting a vulnerability

Use GitHub's
[private vulnerability reporting](https://github.com/corbet-labs/cfetch/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include, when possible:

- the affected cfetch version and build variant;
- platform and agent harness;
- a minimal reproduction or proof of concept;
- the expected impact and affected trust boundary; and
- whether the report contains secrets or private brain content.

Redact credentials, private memory, hostnames, and operator configuration. The
maintainers will acknowledge the report, investigate it privately, coordinate a
fix and release, and credit the reporter unless anonymity is requested.

Security-sensitive areas include hook payload handling, private-region
redaction, path traversal, slice authorization, serving tokens, iroh grants,
endpoint SSRF controls, and accidental indexing or capture of secrets.
