# experiments

Throwaway probes and spikes. Nothing in here is shipped code; anything worth
keeping graduates into `src/` with tests.

`accelerators/openvino_direct_kat.py` tests the unchanged released v1 ONNX
graph through native OpenVINO when ORT's OpenVINO provider cannot own it. It
requires Python packages `openvino`, `numpy`, and `tokenizers` and verifies the
frozen model, tokenizer tensors, execution device, runtime precision summary,
and all 11 final vector hashes:

```console
python experiments/accelerators/openvino_direct_kat.py \
  --model-dir ./cfetch-embeddinggemma-300m-a8w8-v1 \
  --device NPU \
  --output ./openvino-npu.json
```

A mismatch is evidence, so the report is written before the command exits
nonzero. `--inference-precision-hint` exists only for adverse controls; it does
not authorize a vendor-specific v1 graph or codec.
