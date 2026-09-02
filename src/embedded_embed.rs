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

/// Where fastembed caches downloaded models.
pub fn cache_dir() -> PathBuf {
    crate::paths::state_dir().join("models")
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
