# NPU-first embedding admission

cross_backend_eval.py is the release gate for the shared INT8 vector space.
It does not compare every backend with CPU bytes. One admitted NPU artifact is
the anchor; every NPU, GPU, and CPU query/document pairing must retain its
retrieval quality.

Each backend runner writes an .npz file containing:

- metadata: one JSON string with schema_version, profile_id,
  model_revision, vector_encoding, dataset, dataset_revision,
  backend, runtime, artifact_sha256, device, device_class,
  placement_evidence, and accelerated_placement: true;
- queries and documents: the canonical signed INT8 vectors in pinned
  SciFact order;
- queries_repeat and documents_repeat: a second run on the same
  runtime/artifact/device.

The repeat arrays must be byte-identical to their first run. Arrays from
different backends do not have to be byte-identical. Cross-backend equality
and cosine are reported only as diagnostics.

Run the complete release gate with at least one cache from every device class:

    python experiments/embedding-profile/cross_backend_eval.py \
      --backend intel-npu=results/intel-npu.npz \
      --backend nvidia-gpu=results/nvidia-gpu.npz \
      --backend xnnpack-cpu=results/xnnpack-cpu.npz \
      --npu-anchor intel-npu \
      --output results/cross-backend-report.json

The evaluator requires Python with numpy and datasets. It downloads only the
revision-pinned mteb/scifact dataset.
Model execution and hardware placement happen in backend-specific runners,
which remain to be implemented with the native runtime artifacts listed in
release/inference-backends.json.
