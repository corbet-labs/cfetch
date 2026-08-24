//! Verification for the separately distributed EmbeddingGemma model bundle.
//!
//! Model bytes are not covered by cfetch's software license and are not
//! embedded in the remote package. A local-inference build accepts only the
//! artifact and tokenizer hashes frozen by the executable profile.

use std::path::Path;

use anyhow::Context as _;
use sha2::Digest as _;

const MODEL_FILE: &str = "model.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";
const TOKENIZER_CONFIG_FILE: &str = "tokenizer_config.json";
const MODEL_CONFIG_FILE: &str = "config.json";
const SPECIAL_TOKENS_FILE: &str = "special_tokens_map.json";

#[cfg_attr(not(feature = "inference-ort"), allow(dead_code))]
#[derive(Clone)]
pub struct VerifiedBundle {
    pub model: Vec<u8>,
    pub tokenizer: Vec<u8>,
    pub tokenizer_config: Vec<u8>,
    pub model_config: Vec<u8>,
    pub special_tokens_map: Vec<u8>,
}

fn read_regular_file(directory: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    let path = directory.join(name);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("read model bundle entry {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "model bundle entry {} must be a regular file (symlinks are refused)",
        path.display()
    );
    std::fs::read(&path).with_context(|| format!("read model bundle entry {}", path.display()))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn require_hash(name: &str, bytes: &[u8], expected: &str) -> anyhow::Result<()> {
    let actual = sha256(bytes);
    anyhow::ensure!(
        actual == expected,
        "model bundle {name} has SHA-256 {actual}, profile requires {expected}"
    );
    Ok(())
}

impl VerifiedBundle {
    pub fn load(directory: &Path) -> anyhow::Result<Self> {
        let expected_model = crate::embedding_profile::MODEL_ARTIFACT_SHA256.context(
            "the canonical cfetch embedding-v1 artifact has not been published yet; local inference is not admitted",
        )?;
        let metadata = std::fs::metadata(directory)
            .with_context(|| format!("read model bundle directory {}", directory.display()))?;
        anyhow::ensure!(
            metadata.is_dir(),
            "model bundle path {} is not a directory",
            directory.display()
        );

        let model = read_regular_file(directory, MODEL_FILE)?;
        let tokenizer = read_regular_file(directory, TOKENIZER_FILE)?;
        let tokenizer_config = read_regular_file(directory, TOKENIZER_CONFIG_FILE)?;
        let model_config = read_regular_file(directory, MODEL_CONFIG_FILE)?;
        let special_tokens_map = read_regular_file(directory, SPECIAL_TOKENS_FILE)?;

        require_hash(MODEL_FILE, &model, expected_model)?;
        require_hash(
            TOKENIZER_FILE,
            &tokenizer,
            crate::embedding_profile::TOKENIZER_JSON_SHA256,
        )?;
        require_hash(
            TOKENIZER_CONFIG_FILE,
            &tokenizer_config,
            crate::embedding_profile::TOKENIZER_CONFIG_SHA256,
        )?;
        require_hash(
            MODEL_CONFIG_FILE,
            &model_config,
            crate::embedding_profile::MODEL_CONFIG_SHA256,
        )?;
        require_hash(
            SPECIAL_TOKENS_FILE,
            &special_tokens_map,
            crate::embedding_profile::SPECIAL_TOKENS_MAP_SHA256,
        )?;

        Ok(Self {
            model,
            tokenizer,
            tokenizer_config,
            model_config,
            special_tokens_map,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_bundle_fails_before_accepting_arbitrary_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let error = VerifiedBundle::load(temp.path()).err().unwrap().to_string();
        assert!(error.contains("model.onnx"), "{error}");
    }

    #[test]
    fn hash_check_names_both_digests() {
        let error = require_hash("model.onnx", b"wrong", &"0".repeat(64))
            .unwrap_err()
            .to_string();
        assert!(error.contains("model.onnx"));
        assert!(error.contains(&sha256(b"wrong")));
        assert!(error.contains(&"0".repeat(64)));
    }
}
