//! The logical embedding/vector-space contract for cfetch network major v1.
//!
//! The source model and semantic pipeline are shared. Runtime graphs are not:
//! each NPU, GPU, and CPU backend may use the native compiled artifact needed
//! to run efficiently on that device. Backends enter the profile by passing
//! retrieval tests in every query/document pairing, not by reproducing one
//! runtime's bytes.

use crate::config::{EmbeddingsConfig, Precision};

/// The data/network compatibility major. The v1 profile is still a candidate:
/// no released local producer or shared store activated the rejected ORT
/// artifact, so correcting the pre-activation contract does not reinterpret
/// released vectors.
pub const NETWORK_MAJOR: u32 = 1;
pub const PROFILE_ID: &str = "cfetch-embedding-v1";
pub const PROFILE_STATUS: &str = "candidate";
pub const PROFILE_MANIFEST_SHA256: &str =
    "0477729728af5b23ac9969deebd3b7a01f5720ed0f232f41d0da2f651f0d07b5";

/// Immutable upstream source. Backend packages derive native artifacts from
/// this revision; a package-specific graph digest is deliberately not part of
/// the shared vector-space identity.
pub const MODEL: &str = "google/embeddinggemma-300m-qat-q8_0-unquantized";
pub const MODEL_REVISION: &str = "7b5b24595322ab0ea4d08827066860a6df8cb0aa";
pub const MODEL_NUMERIC_FORMAT: &str = "int8-design-center";
pub const ARTIFACT_POLICY: &str = "backend-native-from-pinned-source";
pub const EXECUTION_POLICY: &str = "local-accelerated-npu-gpu-cpu";
pub const REFERENCE_DEVICE_CLASS: &str = "npu";
pub const BACKEND_ADMISSION: &str = "mixed-query-document-retrieval";
pub const BACKEND_REPEATABILITY: &str = "required-per-runtime-artifact-device";
pub const CROSS_BACKEND_EXACT_BYTES: bool = false;

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
pub const SEQUENCE_BUCKETS: &[usize] = &[32, 64, 128, 256, 512, 1024, 2048];
pub const QUERY_PREFIX: &str = "task: search result | query: ";
pub const DOCUMENT_PREFIX: &str = "title: none | text: ";
pub const POOLING: &str = "attention-mask-weighted-mean-include-prompt";
pub const NORMALIZATION: &str = "l2-source-output-then-i8-maxabs-rne-storage";
pub const VECTOR_ENCODING: &str = "signed-int8x768";

/// Serializable form of the vector-space contract. It contains semantic
/// inputs and admission policy, never ORT flags or a vendor artifact hash.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Manifest {
    pub network_major: u32,
    pub profile_id: &'static str,
    pub profile_status: &'static str,
    pub model: &'static str,
    pub model_revision: &'static str,
    pub model_numeric_format: &'static str,
    pub artifact_policy: &'static str,
    pub execution_policy: &'static str,
    pub reference_device_class: &'static str,
    pub backend_admission: &'static str,
    pub backend_repeatability: &'static str,
    pub cross_backend_exact_bytes: bool,
    pub tokenizer_revision: &'static str,
    pub tokenizer_json_sha256: &'static str,
    pub tokenizer_config_sha256: &'static str,
    pub model_config_sha256: &'static str,
    pub special_tokens_map_sha256: &'static str,
    pub max_tokens: usize,
    pub sequence_buckets: &'static [usize],
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
        profile_status: PROFILE_STATUS,
        model: MODEL,
        model_revision: MODEL_REVISION,
        model_numeric_format: MODEL_NUMERIC_FORMAT,
        artifact_policy: ARTIFACT_POLICY,
        execution_policy: EXECUTION_POLICY,
        reference_device_class: REFERENCE_DEVICE_CLASS,
        backend_admission: BACKEND_ADMISSION,
        backend_repeatability: BACKEND_REPEATABILITY,
        cross_backend_exact_bytes: CROSS_BACKEND_EXACT_BYTES,
        tokenizer_revision: MODEL_REVISION,
        tokenizer_json_sha256: TOKENIZER_JSON_SHA256,
        tokenizer_config_sha256: TOKENIZER_CONFIG_SHA256,
        model_config_sha256: MODEL_CONFIG_SHA256,
        special_tokens_map_sha256: SPECIAL_TOKENS_MAP_SHA256,
        max_tokens: MAX_TOKENS,
        sequence_buckets: SEQUENCE_BUCKETS,
        query_prefix: QUERY_PREFIX,
        document_prefix: DOCUMENT_PREFIX,
        pooling: POOLING,
        normalization: NORMALIZATION,
        dimensions: DIMENSIONS,
        vector_encoding: VECTOR_ENCODING,
        vector_bytes: DIMENSIONS,
    }
}

pub fn manifest_sha256() -> String {
    use sha2::Digest as _;

    let bytes = serde_json::to_vec(&manifest()).expect("embedding manifest serializes");
    let digest = format!("{:x}", sha2::Sha256::digest(bytes));
    debug_assert_eq!(digest, PROFILE_MANIFEST_SHA256, "embedding profile digest changed");
    digest
}

/// Refuse semantic drift. Device runtimes and their compiled artifacts are
/// selected outside this contract and therefore are not configuration fields.
pub fn validate(config: &EmbeddingsConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.model == MODEL,
        "cfetch network major {NETWORK_MAJOR} requires embeddings.model={MODEL:?}; changing the source model requires a new network major and re-embedding"
    );
    anyhow::ensure!(
        config.dimensions == DIMENSIONS,
        "cfetch network major {NETWORK_MAJOR} requires {DIMENSIONS} embedding dimensions; changing the width requires a new network major and re-embedding"
    );
    anyhow::ensure!(
        config.precision == Precision::I8,
        "cfetch network major {NETWORK_MAJOR} requires embeddings.precision=\"i8\"; changing vector precision requires a new network major and re-embedding"
    );
    anyhow::ensure!(
        config.query_prefix == QUERY_PREFIX,
        "cfetch network major {NETWORK_MAJOR} fixes embeddings.query_prefix to {QUERY_PREFIX:?}; changing prompts requires a new network major and re-embedding"
    );
    anyhow::ensure!(
        config.document_prefix == DOCUMENT_PREFIX,
        "cfetch network major {NETWORK_MAJOR} fixes embeddings.document_prefix to {DOCUMENT_PREFIX:?}; changing prompts requires a new network major and re-embedding"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_is_an_npu_first_int8_shared_space_not_one_runtime_graph() {
        let profile = manifest();
        assert_eq!(profile.profile_status, "candidate");
        assert_eq!(profile.reference_device_class, "npu");
        assert_eq!(profile.execution_policy, "local-accelerated-npu-gpu-cpu");
        assert_eq!(profile.backend_admission, "mixed-query-document-retrieval");
        assert!(!profile.cross_backend_exact_bytes);
        assert_eq!(profile.artifact_policy, "backend-native-from-pinned-source");
        assert_eq!(profile.model_numeric_format, "int8-design-center");
        assert_eq!(profile.dimensions, 768);
        assert_eq!(profile.vector_encoding, "signed-int8x768");
    }

    #[test]
    fn profile_digest_is_stable_sha256() {
        assert_eq!(manifest_sha256(), PROFILE_MANIFEST_SHA256);
    }

    #[test]
    fn release_registry_keeps_npu_gpu_cpu_order_and_no_unproved_backend() {
        let registry: serde_json::Value =
            serde_json::from_str(include_str!("../release/inference-backends.json")).unwrap();
        assert_eq!(registry["profile_id"], PROFILE_ID);
        assert_eq!(registry["profile_status"], PROFILE_STATUS);
        assert_eq!(registry["reference_device_class"], "npu");
        assert_eq!(
            registry["selection_order"],
            serde_json::json!(["npu", "gpu", "cpu"])
        );
        assert_eq!(registry["remote_policy"], "explicit-only");
        assert_eq!(registry["admitted_backends"], serde_json::json!([]));
    }

    #[test]
    fn semantic_pipeline_changes_are_refused_inside_v1() {
        let valid = EmbeddingsConfig::default();
        validate(&valid).unwrap();

        let mut changed = valid.clone();
        changed.model = "another/model".into();
        assert!(
            validate(&changed)
                .unwrap_err()
                .to_string()
                .contains("new network major")
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
