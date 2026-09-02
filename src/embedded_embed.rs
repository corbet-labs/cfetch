//! Embedded embedding model via ONNX Runtime — no external server needed.
//!
//! Feature-gated behind `embedded-embeddings`. When enabled, cfetch loads a
//! quantized nomic-embed-text ONNX model directly in-process and serves
//! embeddings without Ollama or LM Studio. The model file is downloaded
//! once (`cfetch embed-model download`) and cached in the state directory.
//!
//! The ONNX Runtime handles all model architecture internally — we provide
//! token IDs + attention mask and receive the last hidden state, which we
//! mean-pool (masked) into a single 768-dim vector.

#![cfg(feature = "embedded-embeddings")]

use std::path::{Path, PathBuf};

/// Where the model file lives once downloaded.
pub fn model_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join("nomic-embed-text-v1.5.onnx")
}

/// Whether the model file has been downloaded.
pub fn model_available(state_dir: &Path) -> bool {
    model_path(state_dir).is_file()
}

/// URL for the quantized ONNX export of nomic-embed-text-v1.5.
const MODEL_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/onnx/model.onnx";
const MODEL_SIZE_HINT: u64 = 130 * 1024 * 1024; // ~130 MB

/// Downloads the model to the state directory. Reports progress.
pub fn download_model(state_dir: &Path) -> anyhow::Result<PathBuf> {
    let dest = model_path(state_dir);
    if dest.is_file() {
        println!("model already downloaded: {}", dest.display());
        return Ok(dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
    }
    println!("downloading nomic-embed-text-v1.5 (~{} MB)...", MODEL_SIZE_HINT / 1024 / 1024);
    let response = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .new_agent()
        .get(MODEL_URL)
        .call()
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(&dest)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", dest.display()))?;
    std::io::copy(&mut reader, &mut file)
        .map_err(|e| anyhow::anyhow!("write model: {e}"))?;
    let size = std::fs::metadata(&dest)?.len();
    println!("downloaded {} MB -> {}", size / 1024 / 1024, dest.display());
    Ok(dest)
}

/// A minimal ONNX-based embedding backend. Loads the model once, serves
/// embeddings in-process. Thread-safe via ONNX Runtime's internal session.
pub struct EmbeddedEmbedder {
    #[allow(dead_code)]
    session: ort::session::Session,
}

impl EmbeddedEmbedder {
    /// Loads the ONNX model from the state directory.
    pub fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let path = model_path(state_dir);
        anyhow::ensure!(
            path.is_file(),
            "embedding model not found at {}; run `cfetch embed-model download` first",
            path.display()
        );
        let session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("create ONNX session: {e}"))?
            .commit_from_file(&path)
            .map_err(|e| anyhow::anyhow!("load model {}: {e}", path.display()))?;
        Ok(Self { session })
    }

    /// Embeds one text. Returns a 768-dim f32 vector.
    /// The tokenization is deliberately simple: character-level fallback
    /// for now — a proper BPE tokenizer is the next step.
    pub fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        // For the prototype, use a simple tokenization approach.
        // nomic-embed-text uses a BPE tokenizer with vocab_size ~30528.
        // For now, we hash characters into the vocab range — this produces
        // VALID tensor shapes but NOT semantically meaningful embeddings.
        // A proper tokenizer (tokenizers crate) is the priority follow-up.
        let tokens: Vec<i64> = text
            .bytes()
            .take(512)
            .map(|b| (b as i64) % 30522 + 1)
            .collect();
        let mask: Vec<i64> = vec![1; tokens.len()];

        let input_ids = ort::value::Tensor::from_array((
            vec![1i64, tokens.len() as i64],
            tokens,
        ))
        .map_err(|e| anyhow::anyhow!("create input tensor: {e}"))?;
        let attention_mask = ort::value::Tensor::from_array((
            vec![1i64, mask.len() as i64],
            mask,
        ))
        .map_err(|e| anyhow::anyhow!("create mask tensor: {e}"))?;

        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ])
            .map_err(|e| anyhow::anyhow!("run inference: {e}"))?;

        // Extract the last hidden state [batch, seq, hidden] and mean-pool.
        let (_shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("extract output: {e}"))?;
        let total = data.len();
        anyhow::ensure!(total % 768 == 0, "output length {} is not a multiple of 768", total);
        let hidden_dim = 768usize;
        let seq = total / hidden_dim;
        anyhow::ensure!(seq > 0, "empty output");

        // Masked mean pooling over the sequence dimension.
        let mut pooled = vec![0.0f32; hidden_dim];
        for s in 0..seq {
            for h in 0..hidden_dim {
                pooled[h] += data[s * hidden_dim + h];
            }
        }
        let count = seq.max(1) as f32;
        for p in &mut pooled {
            *p /= count;
        }
        Ok(pooled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_path_is_deterministic() {
        let dir = Path::new("/tmp/state");
        assert!(model_path(dir).ends_with("models/nomic-embed-text-v1.5.onnx"));
    }
}
