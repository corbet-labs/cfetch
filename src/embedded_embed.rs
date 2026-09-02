//! Embedded embedding model via ONNX Runtime — no external server needed.
//!
//! Feature-gated behind `embedded-embeddings`. When enabled, cfetch loads a
//! quantized nomic-embed-text ONNX model directly in-process and serves
//! embeddings without Ollama or LM Studio. The model and tokenizer files
//! are downloaded once (`cfetch embed-model download`) and cached in the
//! state directory.
//!
//! The ONNX Runtime handles the transformer forward pass; the HuggingFace
//! `tokenizers` crate handles BPE tokenization (subword splitting, special
//! tokens [CLS]/[SEP], attention masks). Together they produce semantically
//! meaningful 768-dim embeddings entirely in-process.

#![cfg(feature = "embedded-embeddings")]

use std::path::{Path, PathBuf};

/// Where the ONNX model file lives once downloaded.
pub fn model_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join("nomic-embed-text-v1.5.onnx")
}

/// Where the tokenizer.json file lives once downloaded.
pub fn tokenizer_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join("nomic-embed-text-v1.5-tokenizer.json")
}

/// Whether both the model and tokenizer have been downloaded.
#[allow(dead_code)]
pub fn model_available(state_dir: &Path) -> bool {
    model_path(state_dir).is_file() && tokenizer_path(state_dir).is_file()
}

/// URLs for the ONNX export and tokenizer of nomic-embed-text-v1.5.
const MODEL_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/onnx/model_quantized.onnx";
const TOKENIZER_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main/tokenizer.json";
const MODEL_SIZE_HINT: u64 = 130 * 1024 * 1024; // ~130 MB
const TOKENIZER_SIZE_HINT: u64 = 700 * 1024; // ~700 KB

fn download_to(url: &str, dest: &Path, label: &str) -> anyhow::Result<()> {
    if dest.is_file() {
        println!("{} already downloaded: {}", label, dest.display());
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
    }
    println!("downloading {}...", label);
    let response = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .new_agent()
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("download {} failed: {e}", label))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(dest)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", dest.display()))?;
    std::io::copy(&mut reader, &mut file)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", dest.display()))?;
    let size = std::fs::metadata(dest)?.len();
    println!("  {} MB -> {}", size / 1024 / 1024, dest.display());
    Ok(())
}

/// Downloads the ONNX model and tokenizer to the state directory.
pub fn download_model(state_dir: &Path) -> anyhow::Result<PathBuf> {
    let model = model_path(state_dir);
    let tokenizer = tokenizer_path(state_dir);
    println!("downloading nomic-embed-text-v1.5 (~{} MB total)...",
        (MODEL_SIZE_HINT + TOKENIZER_SIZE_HINT) / 1024 / 1024);
    download_to(MODEL_URL, &model, "ONNX model")?;
    download_to(TOKENIZER_URL, &tokenizer, "tokenizer")?;
    Ok(model)
}

/// An in-process embedding backend: ONNX Runtime for the transformer
/// forward pass, HuggingFace tokenizers for BPE subword tokenization.
pub struct EmbeddedEmbedder {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
}

impl EmbeddedEmbedder {
    /// Loads the ONNX model and tokenizer from the state directory.
    pub fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let model = model_path(state_dir);
        let tok = tokenizer_path(state_dir);
        anyhow::ensure!(
            model.is_file() && tok.is_file(),
            "embedding model not found at {}; run `cfetch embed-model download` first",
            model.display()
        );
        let session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("create ONNX session: {e}"))?
            .commit_from_file(&model)
            .map_err(|e| anyhow::anyhow!("load model {}: {e}", model.display()))?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tok)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tok.display()))?;
        Ok(Self { session, tokenizer })
    }

    /// Embeds one text into a 768-dim f32 vector using proper BPE
    /// tokenization and masked mean pooling of the last hidden state.
    ///
    /// The caller (cfetch's EmbedClient) is responsible for prepending
    /// task-specific prefixes (query vs document) before calling this.
    pub fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        // BPE tokenize with special tokens ([CLS], [SEP] added by the
        // tokenizer's post-processor, matching the model's training).
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
        let type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&t| t as i64).collect();
        anyhow::ensure!(!ids.is_empty(), "tokenizer produced zero tokens for {text:?}");
        anyhow::ensure!(ids.len() <= 512, "input too long: {} tokens (max 512)", ids.len());

        let seq_len = ids.len();
        let input_ids = ort::value::Tensor::from_array((
            vec![1i64, seq_len as i64],
            ids,
        ))
        .map_err(|e| anyhow::anyhow!("create input tensor: {e}"))?;
        let attention_mask = ort::value::Tensor::from_array((
            vec![1i64, seq_len as i64],
            mask.clone(),
        ))
        .map_err(|e| anyhow::anyhow!("create mask tensor: {e}"))?;
        let token_type_ids = ort::value::Tensor::from_array((
            vec![1i64, seq_len as i64],
            type_ids,
        ))
        .map_err(|e| anyhow::anyhow!("create type tensor: {e}"))?;

        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
                "token_type_ids" => token_type_ids,
            ])
            .map_err(|e| anyhow::anyhow!("run inference: {e}"))?;

        // Extract the last hidden state [batch=1, seq, hidden=768] and
        // mean-pool with the attention mask (padding tokens excluded).
        let (_shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("extract output: {e}"))?;
        let total = data.len();
        anyhow::ensure!(total % 768 == 0, "output length {} is not a multiple of 768", total);
        let hidden_dim = 768usize;
        let seq = total / hidden_dim;
        anyhow::ensure!(seq == seq_len, "output seq {} != input seq {}", seq, seq_len);

        // Masked mean pooling: only tokens where mask == 1 contribute.
        let mut pooled = vec![0.0f32; hidden_dim];
        let mut count = 0.0f32;
        for s in 0..seq {
            if mask[s] == 1 {
                count += 1.0;
                for h in 0..hidden_dim {
                    pooled[h] += data[s * hidden_dim + h];
                }
            }
        }
        anyhow::ensure!(count > 0.0, "attention mask is all zeros");
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
        assert!(tokenizer_path(dir).ends_with("models/nomic-embed-text-v1.5-tokenizer.json"));
    }
}
