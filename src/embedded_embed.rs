//! Isolated experimental Nomic embedding probe via ONNX Runtime.
//!
//! Feature-gated behind `embedded-embeddings`. This module exists for local
//! download/status/test diagnostics only. Nomic is not cfetch's canonical
//! EmbeddingGemma profile, and vectors produced here must never enter the
//! local cache or shared vector store.
//!
//! The ONNX Runtime handles the transformer forward pass; the HuggingFace
//! `tokenizers` crate handles BPE tokenization (subword splitting, special
//! tokens [CLS]/[SEP], attention masks). Together they produce semantically
//! meaningful 768-dim embeddings entirely in-process.

#![cfg(feature = "embedded-embeddings")]

use std::path::{Path, PathBuf};
use std::io::{Read as _, Write as _};

use sha2::Digest as _;

/// Where the ONNX model file lives once downloaded.
pub fn model_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join("nomic-embed-text-v1.5.onnx")
}

/// Where the tokenizer.json file lives once downloaded.
pub fn tokenizer_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join("nomic-embed-text-v1.5-tokenizer.json")
}

/// Whether both pinned artifacts are present and verified.
#[allow(dead_code)]
pub fn model_available(state_dir: &Path) -> bool {
    verify_artifact(&model_path(state_dir), MODEL_SIZE, MODEL_SHA256).is_ok()
        && verify_artifact(&tokenizer_path(state_dir), TOKENIZER_SIZE, TOKENIZER_SHA256).is_ok()
}

/// Immutable Hugging Face revision and exact artifact identities.
const MODEL_REVISION: &str = "e9b6763023c676ca8431644204f50c2b100d9aab";
const MODEL_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/e9b6763023c676ca8431644204f50c2b100d9aab/onnx/model_quantized.onnx";
const TOKENIZER_URL: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/e9b6763023c676ca8431644204f50c2b100d9aab/tokenizer.json";
const MODEL_SIZE: u64 = 137_296_292;
const MODEL_SHA256: &str = "b4342336debaea79de872370664b0aaeb67dea4605513d00ee236ea871a81f27";
const TOKENIZER_SIZE: u64 = 711_396;
const TOKENIZER_SHA256: &str =
    "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66";

fn verify_artifact(path: &Path, expected_size: u64, expected_sha256: &str) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| anyhow::anyhow!("inspect {}: {error}", path.display()))?;
    anyhow::ensure!(
        metadata.len() == expected_size,
        "{} has {} bytes, expected {expected_size}",
        path.display(),
        metadata.len()
    );
    let mut file = std::fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("open {}: {error}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| anyhow::anyhow!("hash {}: {error}", path.display()))?;
    let actual = format!("{:x}", hasher.finalize());
    anyhow::ensure!(
        actual == expected_sha256,
        "{} has SHA-256 {actual}, expected {expected_sha256}",
        path.display()
    );
    Ok(())
}

fn download_to(
    url: &str,
    dest: &Path,
    label: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> anyhow::Result<()> {
    if dest.is_file() {
        match verify_artifact(dest, expected_size, expected_sha256) {
            Ok(()) => {
                println!("{} already downloaded and verified: {}", label, dest.display());
                return Ok(());
            }
            Err(error) => println!("{} is invalid ({error}); downloading a verified replacement", label),
        }
    }
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", dest.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| anyhow::anyhow!("create {}: {error}", parent.display()))?;
    println!("downloading {}...", label);
    let response = ureq::Agent::config_builder()
        .max_redirects(5)
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .new_agent()
        .get(url)
        .call()
        .map_err(|error| anyhow::anyhow!("download {label} failed: {error}"))?;
    let mut reader = response.into_body().into_reader();
    let mut tmp = tempfile::Builder::new()
        .prefix(".cfetch-model-download.")
        .tempfile_in(parent)
        .map_err(|error| anyhow::anyhow!("create temporary file in {}: {error}", parent.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| anyhow::anyhow!("read {label}: {error}"))?;
        if count == 0 {
            break;
        }
        downloaded = downloaded
            .checked_add(count as u64)
            .ok_or_else(|| anyhow::anyhow!("download size overflow for {label}"))?;
        anyhow::ensure!(
            downloaded <= expected_size,
            "downloaded {label} exceeds the pinned {expected_size}-byte size"
        );
        tmp.write_all(&buffer[..count])
            .map_err(|error| anyhow::anyhow!("write temporary {label}: {error}"))?;
        hasher.update(&buffer[..count]);
    }
    anyhow::ensure!(
        downloaded == expected_size,
        "downloaded {label} has {downloaded} bytes, expected {expected_size}"
    );
    let actual = format!("{:x}", hasher.finalize());
    anyhow::ensure!(
        actual == expected_sha256,
        "downloaded {label} has SHA-256 {actual}, expected {expected_sha256}"
    );
    tmp.as_file()
        .sync_all()
        .map_err(|error| anyhow::anyhow!("sync temporary {label}: {error}"))?;
    tmp.persist(dest)
        .map_err(|error| error.error)
        .map_err(|error| anyhow::anyhow!("replace {}: {error}", dest.display()))?;
    println!("  {} MB -> {}", downloaded / 1024 / 1024, dest.display());
    Ok(())
}

/// Downloads the ONNX model and tokenizer to the state directory.
pub fn download_model(state_dir: &Path) -> anyhow::Result<PathBuf> {
    let model = model_path(state_dir);
    let tokenizer = tokenizer_path(state_dir);
    println!(
        "downloading pinned nomic-embed-text-v1.5 revision {} (~{} MB total)...",
        MODEL_REVISION,
        (MODEL_SIZE + TOKENIZER_SIZE) / 1024 / 1024
    );
    download_to(MODEL_URL, &model, "ONNX model", MODEL_SIZE, MODEL_SHA256)?;
    download_to(
        TOKENIZER_URL,
        &tokenizer,
        "tokenizer",
        TOKENIZER_SIZE,
        TOKENIZER_SHA256,
    )?;
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
        verify_artifact(&model, MODEL_SIZE, MODEL_SHA256).map_err(|error| {
            anyhow::anyhow!(
                "verify ONNX model {}: {error}; run `cfetch embed-model download`",
                model.display()
            )
        })?;
        verify_artifact(&tok, TOKENIZER_SIZE, TOKENIZER_SHA256).map_err(|error| {
            anyhow::anyhow!(
                "verify tokenizer {}: {error}; run `cfetch embed-model download`",
                tok.display()
            )
        })?;
        let session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("create ONNX session: {e}"))?
            .commit_from_file(&model)
            .map_err(|e| anyhow::anyhow!("load model {}: {e}", model.display()))?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tok)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tok.display()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configure truncation: {e}"))?;
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

    #[test]
    fn artifact_verification_checks_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("artifact");
        std::fs::write(&artifact, b"cfetch").unwrap();
        verify_artifact(
            &artifact,
            6,
            "bab49db60fd8b88607513a9fe8049f460efcecbb58a4fc18191c90bd1e6799d8",
        )
        .unwrap();
        assert!(verify_artifact(&artifact, 7, "irrelevant").is_err());
        assert!(verify_artifact(&artifact, 6, &"0".repeat(64)).is_err());
    }

    #[test]
    fn download_urls_are_revision_pinned() {
        assert!(MODEL_URL.contains(MODEL_REVISION));
        assert!(TOKENIZER_URL.contains(MODEL_REVISION));
        assert!(!MODEL_URL.contains("/resolve/main/"));
        assert!(!TOKENIZER_URL.contains("/resolve/main/"));
    }
}
