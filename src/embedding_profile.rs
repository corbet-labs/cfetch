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

/// One logical quantization across runners. Vendor packages may compile this
/// graph into a native cache, but may not recalibrate it or choose new scales.
pub const MODEL_QUANTIZATION: &str = "a8w8-s8s8-symmetric-qdq-opset18";
pub const MODEL_ARTIFACT_ID: &str = "cfetch-embeddinggemma-300m-a8w8-v1";
pub const MODEL_ARTIFACT_SHA256: Option<&str> =
    Some("ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1");
pub const TOKENIZER_JSON_SHA256: &str =
    "6852f8d561078cc0cebe70ca03c5bfdd0d60a45f9d2e0e1e4cc05b68e9ec329e";
pub const TOKENIZER_CONFIG_SHA256: &str =
    "9076840490613047bc9115963ee96b7702018b0d26ba644240bf856efda93118";
pub const MODEL_CONFIG_SHA256: &str =
    "8f863f76e2d9c710cc833dc92efa898c9adfd41031c786507cc6b0e49c2e3e68";
pub const SPECIAL_TOKENS_MAP_SHA256: &str =
    "2f7b0adf4fb469770bb1490e3e35df87b1dc578246c5e7e6fc76ecf33213a397";
pub const DIMENSIONS: usize = 768;
pub const MAX_TOKENS: usize = 2048;
/// Fixed token shapes make one input independent of the other texts sharing a
/// batch and give accelerator compilers a bounded set of graphs to cache.
pub const SEQUENCE_BUCKETS: &[usize] = &[32, 64, 128, 256, 512, 1024, 2048];
/// v1 chooses interchangeability over bulk throughput: every published vector
/// is inferred with batch dimension one, so an unrelated neighbor cannot
/// select a different accelerator kernel or alter rounding.
pub const INFERENCE_BATCH_SIZE: usize = 1;
/// CPU reductions are single-threaded so core count cannot alter accumulation
/// order. Accelerator providers may schedule their own certified kernels.
pub const ORT_INTRA_THREADS: usize = 1;
pub const ORT_EXECUTION_MODE: &str = "sequential";
pub const QUERY_PREFIX: &str = "task: search result | query: ";
pub const DOCUMENT_PREFIX: &str = "title: none | text: ";
pub const POOLING: &str = "attention-mask-weighted-mean-include-prompt";
pub const GRAPH_OPTIMIZATION: &str = "ort-enable-all";
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
    pub model_artifact_sha256: Option<&'static str>,
    pub tokenizer_revision: &'static str,
    pub tokenizer_json_sha256: &'static str,
    pub tokenizer_config_sha256: &'static str,
    pub model_config_sha256: &'static str,
    pub special_tokens_map_sha256: &'static str,
    pub max_tokens: usize,
    pub sequence_buckets: &'static [usize],
    pub inference_batch_size: usize,
    pub ort_intra_threads: usize,
    pub ort_execution_mode: &'static str,
    pub query_prefix: &'static str,
    pub document_prefix: &'static str,
    pub pooling: &'static str,
    pub graph_optimization: &'static str,
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
        model_artifact_sha256: MODEL_ARTIFACT_SHA256,
        tokenizer_revision: MODEL_REVISION,
        tokenizer_json_sha256: TOKENIZER_JSON_SHA256,
        tokenizer_config_sha256: TOKENIZER_CONFIG_SHA256,
        model_config_sha256: MODEL_CONFIG_SHA256,
        special_tokens_map_sha256: SPECIAL_TOKENS_MAP_SHA256,
        max_tokens: MAX_TOKENS,
        sequence_buckets: SEQUENCE_BUCKETS,
        inference_batch_size: INFERENCE_BATCH_SIZE,
        ort_intra_threads: ORT_INTRA_THREADS,
        ort_execution_mode: ORT_EXECUTION_MODE,
        query_prefix: QUERY_PREFIX,
        document_prefix: DOCUMENT_PREFIX,
        pooling: POOLING,
        graph_optimization: GRAPH_OPTIMIZATION,
        normalization: NORMALIZATION,
        dimensions: DIMENSIONS,
        vector_encoding: VECTOR_ENCODING,
        vector_bytes: DIMENSIONS,
    }
}

/// The sole sequence shape admitted for a tokenized input. Inputs longer than
/// v1's context are truncated to the final bucket by the tokenizer.
#[cfg_attr(not(any(feature = "inference-ort", test)), allow(dead_code))]
pub fn sequence_bucket(token_count: usize) -> usize {
    SEQUENCE_BUCKETS
        .iter()
        .copied()
        .find(|bucket| token_count <= *bucket)
        .unwrap_or(MAX_TOKENS)
}

/// Digest of the complete executable profile, used by stores, endpoint
/// certificates, and accelerator conformance reports. The struct's field
/// order makes the JSON input stable.
pub fn manifest_sha256() -> String {
    use sha2::Digest as _;

    let bytes = serde_json::to_vec(&manifest()).expect("embedding manifest serializes");
    format!("{:x}", sha2::Sha256::digest(bytes))
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
        assert_eq!(m.sequence_buckets, &[32, 64, 128, 256, 512, 1024, 2048]);
        assert_eq!(m.inference_batch_size, 1);
        assert_eq!(m.ort_intra_threads, 1);
        assert_eq!(m.ort_execution_mode, "sequential");
        assert!(m.model.contains("embeddinggemma-300m"));
        assert_eq!(
            m.model_artifact_sha256,
            Some("ed2c0cc371d55d8a6db53308bd923366a93dc5fc9cd8c32e03668ebbc12036e1")
        );
        assert_eq!(m.model_quantization, "a8w8-s8s8-symmetric-qdq-opset18");
    }

    #[test]
    fn sequence_shape_depends_only_on_that_inputs_token_count() {
        assert_eq!(sequence_bucket(0), 32);
        assert_eq!(sequence_bucket(32), 32);
        assert_eq!(sequence_bucket(33), 64);
        assert_eq!(sequence_bucket(2048), 2048);
        assert_eq!(sequence_bucket(9999), 2048);
    }

    #[test]
    fn profile_digest_is_stable_sha256() {
        let digest = manifest_sha256();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
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
