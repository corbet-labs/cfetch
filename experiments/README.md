# experiments

Probes and admission tools live here; none is shipped in the cfetch binary.

- accelerators/apple_compute_probe.swift reports which Apple compute devices
  a hosted runner exposes. Discovery is not backend admission.
- embedding-profile/cross_backend_eval.py evaluates the complete NPU-, GPU-,
  and CPU-query/document matrix for the candidate shared INT8 vector space.

Backend-specific model runners belong beside the matrix evaluator as they are
implemented. They must export the cache contract documented in
embedding-profile/README.md and prove native accelerated placement separately.
