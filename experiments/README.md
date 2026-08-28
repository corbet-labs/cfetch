# experiments

Probes and admission tools live here; none is shipped in the cfetch binary.

- accelerators/apple_compute_probe.swift reports which Apple compute devices
  a hosted runner exposes. Discovery is not backend admission.
- embedding-profile/cross_backend_eval.py evaluates every ordered
  query-backend x document-backend pairing for the candidate shared signed
  INT8x768 output/index space. Every pair is measured against the same fixed
  absolute retrieval floors, then every query backend faces an adversarial
  mixed-document producer matrix; there is no NPU, GPU, or CPU anchor.

Backend-specific model runners belong beside the matrix evaluator as they are
implemented. They must export the cache contract documented in
embedding-profile/README.md and prove native accelerated placement separately.
Their internal precision and artifact format are target-native and are recorded
as evidence, not treated as vector-space identity.
