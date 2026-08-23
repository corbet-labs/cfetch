//! The immutable embedding/network profile for cfetch major v1.
//!
//! A vector is only useful between hosts when every input to its derivation is
//! identical. Backend names are deliberately absent: CPU, GPU and NPU builds
//! may serialize the graph differently, but they must implement this profile
//! and pass the same byte-level conformance vectors before they may publish.

use crate::config::{EmbeddingsConfig, Precision};

/// The cfetch data/network compatibility major. This is independent of the
/// Cargo package version: changing any field below requires major 2 and a
/// coordinated re-embedding of every shared store.
pub const NETWORK_MAJOR: u32 = 1;
pub const PROFILE_ID: &str = "cfetch-embedding-v1";

/// Immutable upstream source. The revision pins the model, tokenizer,
/// SentenceTransformers pooling and projection heads as one artifact.
pub const MODEL: &str = "google/embeddinggemma-300m-qat-q8_0-unquantized";
pub const MODEL_REVISION: &str = "7b5b24595322ab0ea4d08827066860a6df8cb0aa";

/// One logical quantization across runners. Vendor packages may use their
/// native container, but not another numerical scheme.
pub const MODEL_QUANTIZATION: &str = "xint8-w8a8-symmetric-power-of-two-scales";
pub const MODEL_ARTIFACT_ID: &str = "cfetch-embeddinggemma-300m-xint8-v1";
pub const DIMENSIONS: usize = 768;
pub const MAX_TOKENS: usize = 2048;
pub const QUERY_PREFIX: &str = "task: search result | query: ";
pub const DOCUMENT_PREFIX: &str = "title: none | text: ";
pub const POOLING: &str = "attention-mask-weighted-mean-include-prompt";
pub const NORMALIZATION: &str = "l2-then-i8-maxabs-rne";
pub const VECTOR_ENCODING: &str = "signed-int8x768";

/// Serializable and human-readable form of the compatibility contract.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Manifest {
    pub network_major: u32,
    pub profile_id: &'static str,
    pub model: &'static str,
    pub model_revision: &'static str,
    pub model_quantization: &'static str,
    pub model_artifact_id: &'static str,
    pub tokenizer_revision: &'static str,
    pub max_tokens: usize,
    pub query_prefix: &'static str,
    pub document_prefix: &'static str,
    pub pooling: &'static str,
    pub normalization: &'static str,
    pub dimensions: usize,
    pub vector_encoding: &'static str,
    pub vector_bytes: usize,
}

pub const fn manifest() -> Manifest {
    Manifest {
        network_major: NETWORK_MAJOR,
        profile_id: PROFILE_ID,
        model: MODEL,
        model_revision: MODEL_REVISION,
        model_quantization: MODEL_QUANTIZATION,
        model_artifact_id: MODEL_ARTIFACT_ID,
        tokenizer_revision: MODEL_REVISION,
        max_tokens: MAX_TOKENS,
        query_prefix: QUERY_PREFIX,
        document_prefix: DOCUMENT_PREFIX,
        pooling: POOLING,
        normalization: NORMALIZATION,
        dimensions: DIMENSIONS,
        vector_encoding: VECTOR_ENCODING,
        vector_bytes: DIMENSIONS,
    }
}

/// Refuse configuration drift instead of creating plausible but incompatible
/// vectors. Endpoint location and credentials remain host-local; the pipeline
/// itself is not configurable inside one network major.
pub fn validate(config: &EmbeddingsConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.model == MODEL,
        "cfetch network major {NETWORK_MAJOR} requires embeddings.model={MODEL:?}; \
         changing the model requires a new cfetch network major and re-embedding"
    );
    anyhow::ensure!(
        config.dimensions == DIMENSIONS,
        "cfetch network major {NETWORK_MAJOR} requires {DIMENSIONS} embedding dimensions; \
         changing the width requires a new cfetch network major and re-embedding"
    );
    anyhow::ensure!(
        config.precision == Precision::I8,
        "cfetch network major {NETWORK_MAJOR} requires embeddings.precision=\"i8\"; \
         changing vector precision requires a new cfetch network major and re-embedding"
    );
    anyhow::ensure!(
        config.query_prefix == QUERY_PREFIX,
        "cfetch network major {NETWORK_MAJOR} fixes embeddings.query_prefix to {QUERY_PREFIX:?}; \
         changing prompts requires a new cfetch network major and re-embedding"
    );
    anyhow::ensure!(
        config.document_prefix == DOCUMENT_PREFIX,
        "cfetch network major {NETWORK_MAJOR} fixes embeddings.document_prefix to {DOCUMENT_PREFIX:?}; \
         changing prompts requires a new cfetch network major and re-embedding"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_manifest_is_the_frozen_768_byte_int8_contract() {
        let m = manifest();
        assert_eq!(m.network_major, 1);
        assert_eq!(m.dimensions, 768);
        assert_eq!(m.vector_bytes, 768);
        assert_eq!(m.vector_encoding, "signed-int8x768");
        assert!(m.model.contains("embeddinggemma-300m"));
        assert_eq!(
            m.model_quantization,
            "xint8-w8a8-symmetric-power-of-two-scales"
        );
    }

    #[test]
    fn every_pipeline_change_is_refused_inside_v1() {
        let valid = EmbeddingsConfig::default();
        validate(&valid).unwrap();

        let mut changed = valid.clone();
        changed.model = "another/model".into();
        assert!(
            validate(&changed)
                .unwrap_err()
                .to_string()
                .contains("new cfetch network major")
        );

        let mut changed = valid.clone();
        changed.dimensions = 256;
        assert!(
            validate(&changed)
                .unwrap_err()
                .to_string()
                .contains("re-embedding")
        );

        let mut changed = valid;
        changed.document_prefix.clear();
        assert!(validate(&changed).is_err());
    }
}
