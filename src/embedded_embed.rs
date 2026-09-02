//! Embedded embedding and reranking via fastembed (ONNX Runtime).
//!
//! Feature-gated behind `embedded-embeddings`. Uses the `fastembed` crate
//! which handles model download, tokenization, pooling, caching, and ONNX
//! inference — all in-process with zero external server dependencies.
//!
//! Supported models include multilingual-e5-base (768-dim, 100+ languages,
//! for German/English semantic recall), embeddinggemma-300m (cfetch's
//! canonical profile), BGE-M3 (multi-vector), and Jina reranker v2
//! multilingual (local cross-encoder reranking).

#![cfg(feature = "embedded-embeddings")]

use std::path::PathBuf;

/// Default embedding model: multilingual-e5-base (278M, 768 dims).
/// Covers 100+ languages including German and English.
pub const DEFAULT_EMBEDDING_MODEL: fastembed::EmbeddingModel =
    fastembed::EmbeddingModel::MultilingualE5Base;

/// Default reranking model: Jina reranker v2 multilingual.
/// Cross-encoder that scores query-document pairs locally.
#[allow(dead_code)]
pub const DEFAULT_RERANKER_MODEL: fastembed::RerankerModel =
    fastembed::RerankerModel::JINARerankerV2BaseMultiligual;

/// Human-readable name of the default reranker (for status output).
pub const DEFAULT_RERANKER_MODEL_NAME: &str = "jina-reranker-v2-base-multilingual";

/// Where fastembed caches downloaded models.
pub fn cache_dir() -> PathBuf {
    crate::paths::state_dir().join("models")
}

/// Reads the shared vector store's model metadata (without loading it).
/// Returns None if no shared store exists.
/// The filename encodes the spec: network1-<profile>-<model>-<dim>-<precision>-<hash>.idx
pub fn shared_store_model() -> Option<(String, usize)> {
    let store_dir = crate::paths::shared_vector_dir(&crate::paths::default_brain_root());
    let entries = std::fs::read_dir(&store_dir).ok()?;
    let idx_file = entries.flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "idx"))?;
    let filename = idx_file.file_name()?.to_str()?;
    let parts: Vec<&str> = filename.split('-').collect();
    // Expected: [network1, profile, ..., model_name, dim, precision, hash.idx]
    // Find the dim part (a pure number) near the end
    if parts.len() < 4 {
        return None;
    }
    let dim: usize = parts.get(parts.len().saturating_sub(3))?.parse().ok()?;
    let model = parts[2..parts.len().saturating_sub(3)].join("-");
    Some((model, dim))
}

/// Maps a model name (from the shared store) to a fastembed model.
/// Handles both the canonical HuggingFace name and the filename variant.
pub fn model_from_name(name: &str) -> Option<fastembed::EmbeddingModel> {
    use fastembed::EmbeddingModel::*;
    let lower = name.to_lowercase().replace('_', "/");
    if lower.contains("multilingual-e5-base") {
        Some(MultilingualE5Base)
    } else if lower.contains("multilingual-e5-large") {
        Some(MultilingualE5Large)
    } else if lower.contains("multilingual-e5-small") {
        Some(MultilingualE5Small)
    } else if lower.contains("embeddinggemma") {
        Some(EmbeddingGemma300M)
    } else if lower.contains("bge-small-en") {
        Some(BGESmallENV15)
    } else if lower.contains("bge-large-en") {
        Some(BGELargeENV15)
    } else if lower.contains("nomic-embed-text") {
        Some(NomicEmbedTextV15)
    } else if lower.contains("minilm-l6") {
        Some(AllMiniLML6V2)
    } else {
        None
    }
}

/// Checks whether the local model is compatible with the shared vector store.
/// Returns a human-readable report and whether auto-switch is possible.
pub fn check_compatibility() -> CompatibilityReport {
    let Some((shared_model, shared_dim)) = shared_store_model() else {
        return CompatibilityReport {
            status: CompatStatus::NoSharedStore,
            shared_model: String::new(),
            shared_dim: 0,
            local_model: "multilingual-e5-base (default)".to_string(),
            can_auto_switch: false,
            fastembed_variant: None,
        };
    };
    // For now, the local model is always the default (MultilingualE5Base).
    // When per-model config is added, read from config here.
    let local = "multilingual-e5-base";
    let compatible = shared_model.contains("multilingual-e5-base")
        || shared_model.replace('_', "/").contains("multilingual/e5/base");
    let fastembed_variant = model_from_name(&shared_model);
    CompatibilityReport {
        status: if compatible { CompatStatus::Compatible } else { CompatStatus::Incompatible },
        shared_model,
        shared_dim,
        local_model: local.to_string(),
        can_auto_switch: fastembed_variant.is_some(),
        fastembed_variant,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompatStatus {
    NoSharedStore,
    Compatible,
    Incompatible,
}

#[derive(Debug, Clone)]
pub struct CompatibilityReport {
    pub status: CompatStatus,
    pub shared_model: String,
    pub shared_dim: usize,
    pub local_model: String,
    pub can_auto_switch: bool,
    pub fastembed_variant: Option<fastembed::EmbeddingModel>,
}

/// An in-process embedding backend using fastembed.
/// Handles model download (first use), tokenization, and inference.
pub struct EmbeddedEmbedder {
    model: fastembed::TextEmbedding,
}

impl EmbeddedEmbedder {
    /// Loads the embedding model, downloading it on first use.
    pub fn load() -> anyhow::Result<Self> {
        let model = fastembed::TextEmbedding::try_new(
            fastembed::TextInitOptions::new(DEFAULT_EMBEDDING_MODEL)
                .with_cache_dir(cache_dir())
                .with_show_download_progress(true),
        )
        .map_err(|e| anyhow::anyhow!("load embedding model: {e}"))?;
        Ok(Self { model })
    }

    /// Embeds texts into 768-dim f32 vectors.
    /// fastembed handles tokenization, truncation, and mean pooling.
    pub fn embed(&mut self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        self.model
            .embed(texts, None)
            .map_err(|e| anyhow::anyhow!("embed: {e}"))
    }
}

/// An in-process reranker using fastembed.
/// Scores query-document pairs with a local cross-encoder.
#[allow(dead_code)]
pub struct EmbeddedReranker {
    model: fastembed::TextRerank,
}

#[allow(dead_code)]
impl EmbeddedReranker {
    /// Loads the reranker model, downloading it on first use.
    pub fn load() -> anyhow::Result<Self> {
        let model = fastembed::TextRerank::try_new(
            fastembed::RerankInitOptions::new(DEFAULT_RERANKER_MODEL)
                .with_cache_dir(cache_dir())
                .with_show_download_progress(true),
        )
        .map_err(|e| anyhow::anyhow!("load reranker: {e}"))?;
        Ok(Self { model })
    }

    /// Reranks documents against a query. Returns (index, score) sorted by
    /// relevance (best first). If `return_text` is true, includes content.
    pub fn rerank(
        &mut self,
        query: &str,
        documents: &[&str],
        return_text: bool,
    ) -> anyhow::Result<Vec<(usize, f32, Option<String>)>> {
        let results = self
            .model
            .rerank(query, documents, return_text, None)
            .map_err(|e| anyhow::anyhow!("rerank: {e}"))?;
        Ok(results
            .into_iter()
            .map(|r| (r.index, r.score, r.document))
            .collect())
    }

    /// Scores every document against the query, one score per input document
    /// in INPUT order — the same contract as `RerankClient::rank`.
    pub fn rank(&mut self, query: &str, documents: &[&str]) -> anyhow::Result<Vec<f32>> {
        let results = self.model.rerank(query, documents, false, None)
            .map_err(|e| anyhow::anyhow!("rerank: {e}"))?;
        let mut scores = vec![f32::MIN; documents.len()];
        for r in results {
            if let Some(slot) = scores.get_mut(r.index) {
                *slot = r.score;
            }
        }
        Ok(scores)
    }
}

/// Lists all available embedding models (for `cfetch embed-model list`).
#[allow(dead_code)]
pub fn available_models() -> Vec<(&'static str, &'static str)> {
    vec![
        ("MultilingualE5Base", "278M, 768d, 100+ languages (default)"),
        ("MultilingualE5Large", "560M, 1024d, 100+ languages"),
        ("MultilingualE5Small", "118M, 384d, 100+ languages"),
        ("EmbeddingGemma300M", "300M, 768d, cfetch canonical"),
        ("BGESmallENV15", "33M, 384d, English"),
        ("BGELargeENV15", "335M, 1024d, English"),
        ("AllMiniLML6V2", "22M, 384d, English (fastest)"),
        ("NomicEmbedTextV15", "137M, 768d, English"),
        ("MultilingualE5BaseQ", "quantized, 768d, 100+ languages"),
        ("EmbeddingGemma300MQ4", "4-bit, 768d, cfetch canonical"),
    ]
}

/// Lists all available reranker models.
#[allow(dead_code)]
pub fn available_rerankers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("BGERerankerBase", "English cross-encoder"),
        ("BGERerankerV2M3", "Multilingual cross-encoder"),
        ("JinaRerankerV2BaseMultilingual", "Multilingual (default)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_is_under_state() {
        assert!(cache_dir().starts_with(crate::paths::state_dir()));
    }
}
