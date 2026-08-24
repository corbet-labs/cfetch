//! Verified local embeddings through FastEmbed's tokenizer/pipeline and ONNX
//! Runtime's execution providers.
//!
//! This module is compiled only into inference variants. It never downloads a
//! model: [`crate::model_artifact::VerifiedBundle`] admits the exact v1 bytes
//! before ORT sees them.

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;
use std::{fs::File, io::Read as _};

use anyhow::Context as _;
use fastembed::{
    InitOptionsUserDefined, OutputKey, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
#[cfg(feature = "inference-vitis")]
use ort::ep::ArbitrarilyConfigurableExecutionProvider as _;
use ort::ep::ExecutionProviderDispatch;
use sha2::Digest as _;
use tokenizers::{PaddingStrategy, TruncationParams};

use crate::model_artifact::VerifiedBundle;

pub struct LocalEmbedder {
    provider: &'static str,
    device_class: &'static str,
    length_tokenizer: tokenizers::Tokenizer,
    model: Mutex<ModelState>,
}

#[derive(Debug, serde::Serialize)]
pub struct CertificationReport {
    pub schema: u32,
    pub cfetch_version: &'static str,
    pub network_major: u32,
    pub profile_id: &'static str,
    pub profile_manifest_sha256: String,
    pub artifact_id: &'static str,
    pub artifact_sha256: &'static str,
    pub model_quantization: &'static str,
    pub vector_encoding: &'static str,
    pub provider: &'static str,
    pub device_class: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub ort_crate: &'static str,
    pub onnxruntime_build_info: &'static str,
    pub onnxruntime_distribution: &'static str,
    pub onnxruntime_archive_sha256: &'static str,
    pub onnxruntime_library_sha256: String,
    pub fastembed: &'static str,
    pub graph_optimization: &'static str,
    pub inference_batch_size: usize,
    pub ort_intra_threads: usize,
    pub ort_execution_mode: &'static str,
    pub cpu_fallback_disabled: bool,
    pub graph_ownership_enforced: bool,
    pub int8_kernel_evidence: &'static str,
    pub exact_vector_conformance: bool,
    pub producer_eligible_without_external_review: bool,
    pub known_answers: Vec<KnownAnswerResult>,
    pub elapsed_ms: u128,
}

#[derive(Debug, serde::Serialize)]
pub struct KnownAnswerResult {
    pub label: &'static str,
    pub kind: &'static str,
    pub sequence_bucket: usize,
    pub vector_sha256: String,
    pub vector_hex: String,
    pub model_output_f32_le_sha256: String,
    pub model_output_preview: Vec<f32>,
    pub expected_vector_sha256: &'static str,
    pub input_ids_sha256: String,
    pub attention_mask_sha256: String,
    pub passed: bool,
    pub latency_ms: u128,
}

struct KnownAnswer {
    label: &'static str,
    kind: &'static str,
    seed: &'static str,
    repeats: usize,
    expected_bucket: usize,
    expected_sha256: &'static str,
}

const KNOWN_ANSWERS: &[KnownAnswer] = &[
    KnownAnswer {
        label: "short-query",
        kind: "query",
        seed: "Which files define cfetch's embedding compatibility boundary?",
        repeats: 1,
        expected_bucket: 32,
        expected_sha256: "7e67364be4c574340d3693b2743c32d0f8841bf9e723bda821d4ab0c85e32984",
    },
    KnownAnswer {
        label: "profile-document",
        kind: "document",
        seed: "The embedding profile pins the model, tokenizer, prompts, pooling, dimensions, quantization, and vector codec.",
        repeats: 1,
        expected_bucket: 32,
        expected_sha256: "f9fde22be9dce6cee9ff9e4bad7d6ad36f8d47e7fe697f3536ac3fa822985ce7",
    },
    KnownAnswer {
        label: "source-code",
        kind: "document",
        seed: "fn main() { println!(\"deterministic vectors\"); }",
        repeats: 1,
        expected_bucket: 32,
        expected_sha256: "9c4f0753a787d4e28feaa6e02b445ff4e53e4402343e6d1390c19bf7ceddf4d4",
    },
    KnownAnswer {
        label: "german-query",
        kind: "query",
        seed: "Wie werden inkompatible Vektoren im Netzwerk verhindert?",
        repeats: 1,
        expected_bucket: 32,
        expected_sha256: "e1b939cf65256191e909881ec3067dedc7ddd3f0f92541c6183d6040e8da97f0",
    },
    KnownAnswer {
        label: "japanese-document",
        kind: "document",
        seed: "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        repeats: 1,
        expected_bucket: 32,
        expected_sha256: "44f0b8754226feae1b0ee996a77f2288c893871c9ae326625c3cdd0b61410f03",
    },
    KnownAnswer {
        label: "bucket-64",
        kind: "document",
        seed: "The canonical vector store rejects conflicting bytes for identical content.",
        repeats: 3,
        expected_bucket: 64,
        expected_sha256: "f29f4ace39dce50732523a425194ccb0a1806c20f38f594658cd38653ff8a95e",
    },
    KnownAnswer {
        label: "bucket-128",
        kind: "document",
        seed: "fn verify(hash: &str, vector: &[i8]) { assert_eq!(vector.len(), 768); }",
        repeats: 2,
        expected_bucket: 128,
        expected_sha256: "074b2c41449eab85b0adcdc7258c6a01946c8c95245de7d05ccb6d2bfd134554",
    },
    KnownAnswer {
        label: "bucket-256",
        kind: "query",
        seed: "Warum müssen alle Teilnehmer genau dasselbe Einbettungsprofil verwenden?",
        repeats: 9,
        expected_bucket: 256,
        expected_sha256: "9c0107c38c797b699877ab5776416dcef11f8d4cf4643099a6ce71be88489eca",
    },
    KnownAnswer {
        label: "bucket-512",
        kind: "document",
        seed: "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        repeats: 14,
        expected_bucket: 512,
        expected_sha256: "d9722be728ca18586796b418c53cf48ae8aa477346c48f5ab8b075a9b60f8c50",
    },
    KnownAnswer {
        label: "bucket-1024",
        kind: "document",
        seed: "يجب أن تستخدم جميع الأجهزة النموذج نفسه وخط المعالجة نفسه حتى تبقى المتجهات قابلة للتبادل.",
        repeats: 19,
        expected_bucket: 1024,
        expected_sha256: "97cb541fd8bf2b9a996e305743dc63cad654a41e0d26d3880559a49331761653",
    },
    KnownAnswer {
        label: "bucket-2048",
        kind: "document",
        seed: "{\"network_major\":1,\"dimensions\":768,\"precision\":\"int8\",\"compatible\":true}",
        repeats: 45,
        expected_bucket: 2048,
        expected_sha256: "f6ff965cf3907110de5569bf441275b8423567d0b74eb1e0557858fc2c8064da",
    },
];

enum ModelState {
    Dynamic(TextEmbedding),
    /// Accelerator compilers receive a fully static batch/sequence graph.
    /// Keep only the current bucket's session: seven simultaneous optimized
    /// copies consumed roughly 8 GiB in the adverse CPU probe, while
    /// sequential specialization produced the exact same KAT bytes for every
    /// bucket.
    StaticAccelerator {
        bundle: VerifiedBundle,
        current: Option<(usize, TextEmbedding)>,
    },
}

impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEmbedder")
            .field("provider", &self.provider)
            .field("device_class", &self.device_class)
            .finish_non_exhaustive()
    }
}

impl LocalEmbedder {
    fn load_unadmitted(model_dir: &Path, requested_provider: &str) -> anyhow::Result<Self> {
        let bundle = VerifiedBundle::load(model_dir)?;
        let (provider, device_class, dispatch) = execution_provider(requested_provider, None)?;
        let mut length_tokenizer = tokenizers::Tokenizer::from_bytes(bundle.tokenizer.clone())
            .map_err(|error| anyhow::anyhow!("load verified tokenizer: {error}"))?;
        length_tokenizer.with_padding(None);
        length_tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: crate::embedding_profile::MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|error| anyhow::anyhow!("configure verified tokenizer truncation: {error}"))?;

        let model = if provider != "cpu" {
            ModelState::StaticAccelerator {
                bundle,
                current: None,
            }
        } else {
            ModelState::Dynamic(build_model(bundle, provider, dispatch, None)?)
        };

        Ok(Self {
            provider,
            device_class,
            length_tokenizer,
            model: Mutex::new(model),
        })
    }

    /// Load a local producer only after this package/runtime passes the
    /// released byte KAT. Certification itself deliberately bypasses this
    /// admission so new and incompatible hardware can still emit evidence.
    pub fn load_for_production(model_dir: &Path, requested_provider: &str) -> anyhow::Result<Self> {
        let embedder = Self::load_unadmitted(model_dir, requested_provider)?;
        let report = certify_loaded(&embedder, Instant::now())?;
        ensure_production_admission(
            report.provider,
            report.exact_vector_conformance,
            report.producer_eligible_without_external_review,
        )?;
        Ok(embedder)
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    pub fn device_class(&self) -> &'static str {
        self.device_class
    }

    pub fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut state = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("local embedding model lock poisoned"))?;
        let mut outputs = Vec::with_capacity(texts.len());
        for text in texts {
            let token_count = self
                .length_tokenizer
                .encode(*text, true)
                .map_err(|error| anyhow::anyhow!("tokenize verified embedding input: {error}"))?
                .len();
            let bucket = crate::embedding_profile::sequence_bucket(token_count);
            let model = match &mut *state {
                ModelState::Dynamic(model) => model,
                ModelState::StaticAccelerator { bundle, current } => {
                    if current.as_ref().map(|(shape, _)| *shape) != Some(bucket) {
                        let (_, _, dispatch) = execution_provider(self.provider, Some(bucket))?;
                        *current = Some((
                            bucket,
                            build_model(bundle.clone(), self.provider, dispatch, Some(bucket))?,
                        ));
                    }
                    &mut current.as_mut().expect("accelerator session initialized").1
                }
            };
            set_fixed_padding(model, bucket)?;
            let batches = model
                .transform(
                    [*text],
                    Some(crate::embedding_profile::INFERENCE_BATCH_SIZE),
                )
                .with_context(|| {
                    format!(
                        "run verified model with ORT {} provider at sequence bucket {bucket}",
                        self.provider
                    )
                })?;
            let raw = batches.into_raw();
            anyhow::ensure!(
                raw.len() == 1,
                "verified local model returned {} batches",
                raw.len()
            );
            let precedence = &OutputKey::ByName("sentence_embedding");
            let selected = raw[0]
                .select_output(&precedence)
                .context("select frozen sentence_embedding output")?;
            anyhow::ensure!(
                selected.ndim() == 2 && selected.shape()[0] == 1,
                "verified sentence_embedding output has shape {:?}, expected [1, {}]",
                selected.shape(),
                crate::embedding_profile::DIMENSIONS
            );
            outputs.push(
                selected
                    .rows()
                    .into_iter()
                    .next()
                    .context("verified sentence_embedding output has no row")?
                    .to_vec(),
            );
        }
        Ok(outputs)
    }

    fn bucket_for_text(&self, text: &str) -> anyhow::Result<usize> {
        let token_count = self
            .length_tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("tokenize known-answer input: {error}"))?
            .len();
        Ok(crate::embedding_profile::sequence_bucket(token_count))
    }

    fn token_tensor_digests(&self, text: &str, bucket: usize) -> anyhow::Result<(String, String)> {
        let mut state = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("local embedding model lock poisoned"))?;
        let model = match &mut *state {
            ModelState::Dynamic(model) => model,
            ModelState::StaticAccelerator { current, .. } => {
                &mut current
                    .as_mut()
                    .context("accelerator known-answer session was not initialized")?
                    .1
            }
        };
        set_fixed_padding(model, bucket)?;
        let encoding = model
            .tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("tokenize known-answer tensors: {error}"))?;
        anyhow::ensure!(
            encoding.len() == bucket,
            "known-answer tokenizer emitted {} values for bucket {bucket}",
            encoding.len()
        );
        let ids = encoding
            .get_ids()
            .iter()
            .flat_map(|value| i64::from(*value).to_le_bytes())
            .collect::<Vec<_>>();
        let mask = encoding
            .get_attention_mask()
            .iter()
            .flat_map(|value| i64::from(*value).to_le_bytes())
            .collect::<Vec<_>>();
        Ok((
            format!("{:x}", sha2::Sha256::digest(ids)),
            format!("{:x}", sha2::Sha256::digest(mask)),
        ))
    }
}

fn ensure_production_admission(provider: &str, exact: bool, eligible: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        exact,
        "{provider} provider output is incompatible with {} on this host; local production is disabled, but shared/remote vectors remain usable",
        crate::embedding_profile::PROFILE_ID
    );
    anyhow::ensure!(
        eligible,
        "{provider} provider has no production admission for this runtime/device; run `cfetch inference-certify` and submit the report plus required placement evidence"
    );
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn runtime_library_sha256() -> anyhow::Result<String> {
    let Some(path) = std::env::var_os("ORT_DYLIB_PATH") else {
        return Ok("unrecorded".into());
    };
    let canonical =
        std::fs::canonicalize(&path).context("resolve packaged ONNX Runtime library")?;
    anyhow::ensure!(
        canonical.is_file(),
        "packaged ONNX Runtime library is not a regular file"
    );
    let mut file = File::open(&canonical).context("open packaged ONNX Runtime library")?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .context("hash packaged ONNX Runtime library")?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Run the released public known-answer corpus through the actual
/// FastEmbed/ORT path. A successful accelerator session also proves that ORT
/// assigned the complete graph without CPU fallback; vendor profiler evidence
/// is still required to prove that its kernels did not dequantize learned
/// regions internally.
pub fn certify(model_dir: &Path, requested_provider: &str) -> anyhow::Result<CertificationReport> {
    let started = Instant::now();
    let embedder = LocalEmbedder::load_unadmitted(model_dir, requested_provider)?;
    certify_loaded(&embedder, started)
}

fn certify_loaded(
    embedder: &LocalEmbedder,
    started: Instant,
) -> anyhow::Result<CertificationReport> {
    let mut results = Vec::with_capacity(KNOWN_ANSWERS.len());
    for known in KNOWN_ANSWERS {
        let body = std::iter::repeat_n(known.seed, known.repeats)
            .collect::<Vec<_>>()
            .join("\n");
        let prefix = if known.kind == "query" {
            crate::embedding_profile::QUERY_PREFIX
        } else {
            crate::embedding_profile::DOCUMENT_PREFIX
        };
        let input = format!("{prefix}{body}");
        let bucket = embedder.bucket_for_text(&input)?;
        anyhow::ensure!(
            bucket == known.expected_bucket,
            "known-answer {} tokenized to bucket {bucket}, profile requires {}",
            known.label,
            known.expected_bucket
        );
        let run_started = Instant::now();
        let vector = embedder
            .embed(&[input.as_str()])?
            .pop()
            .context("known-answer execution returned no vector")?;
        anyhow::ensure!(
            vector.len() == crate::embedding_profile::DIMENSIONS,
            "known-answer {} returned {} dimensions, profile requires {}",
            known.label,
            vector.len(),
            crate::embedding_profile::DIMENSIONS
        );
        anyhow::ensure!(
            vector.iter().all(|component| component.is_finite()),
            "known-answer {} returned a non-finite component",
            known.label
        );
        let (input_ids_sha256, attention_mask_sha256) =
            embedder.token_tensor_digests(&input, bucket)?;
        let raw_bytes = vector
            .iter()
            .flat_map(|component| component.to_le_bytes())
            .collect::<Vec<_>>();
        let raw_digest = format!("{:x}", sha2::Sha256::digest(raw_bytes));
        let preview = vector.iter().take(8).copied().collect();
        let bytes = crate::index::vec_to_blob(&vector, crate::config::Precision::I8);
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        results.push(KnownAnswerResult {
            label: known.label,
            kind: known.kind,
            sequence_bucket: bucket,
            passed: digest == known.expected_sha256,
            vector_sha256: digest,
            vector_hex: hex_bytes(&bytes),
            model_output_f32_le_sha256: raw_digest,
            model_output_preview: preview,
            expected_vector_sha256: known.expected_sha256,
            input_ids_sha256,
            attention_mask_sha256,
            latency_ms: run_started.elapsed().as_millis(),
        });
    }
    let exact = results.iter().all(|result| result.passed);
    let accelerator = embedder.provider() != "cpu";
    let runtime_distribution = option_env!("CFETCH_ORT_DISTRIBUTION").unwrap_or("unrecorded");
    let runtime_archive_sha256 = option_env!("CFETCH_ORT_ARCHIVE_SHA256").unwrap_or("unrecorded");
    let runtime_library_sha256 = runtime_library_sha256()?;
    let admitted_cpu_runtime = runtime_distribution == "microsoft-github-release-v1.28.0"
        && matches!(
            (
                std::env::consts::OS,
                std::env::consts::ARCH,
                runtime_archive_sha256,
                runtime_library_sha256.as_str(),
            ),
            (
                "linux",
                "x86_64",
                "a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407",
                "1461ef7cc3d9e49982591721683cc3e3a55580aeca9a5254e7aac47b75ee4bab",
            ) | (
                "linux",
                "aarch64",
                "e15ff8b5d85afe6c144d97c6fd432254bf76a219daaf17658087d6ecb3e8f0bb",
                "f1ec1a08eb99bd6e5401340f0a2b101381bf4694415480291dc13bcaa30f9ec7",
            ) | (
                "macos",
                "aarch64",
                "1268b359718099bde2cedb55787f182a130067bc4f31e8c88478c445b850d3d8",
                "dc19bbcb2f5c9fb3c68b4f9248aa0a35065ff702c5dbeae75eac54a74da97b6d",
            )
        );
    let artifact_sha256 = crate::embedding_profile::MODEL_ARTIFACT_SHA256
        .expect("published profile always names its artifact");
    Ok(CertificationReport {
        schema: 1,
        cfetch_version: env!("CARGO_PKG_VERSION"),
        network_major: crate::embedding_profile::NETWORK_MAJOR,
        profile_id: crate::embedding_profile::PROFILE_ID,
        profile_manifest_sha256: crate::embedding_profile::manifest_sha256(),
        artifact_id: crate::embedding_profile::MODEL_ARTIFACT_ID,
        artifact_sha256,
        model_quantization: crate::embedding_profile::MODEL_QUANTIZATION,
        vector_encoding: crate::embedding_profile::VECTOR_ENCODING,
        provider: embedder.provider(),
        device_class: embedder.device_class(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        ort_crate: "2.0.0-rc.13 (API 18)",
        onnxruntime_build_info: ort::info(),
        onnxruntime_distribution: runtime_distribution,
        onnxruntime_archive_sha256: runtime_archive_sha256,
        onnxruntime_library_sha256: runtime_library_sha256,
        fastembed: "6.0.0 + cfetch session-controls 234fc39af5b010de0cb0c5688d138492e690fa68",
        graph_optimization: crate::embedding_profile::GRAPH_OPTIMIZATION,
        inference_batch_size: crate::embedding_profile::INFERENCE_BATCH_SIZE,
        ort_intra_threads: crate::embedding_profile::ORT_INTRA_THREADS,
        ort_execution_mode: crate::embedding_profile::ORT_EXECUTION_MODE,
        cpu_fallback_disabled: accelerator,
        graph_ownership_enforced: accelerator,
        int8_kernel_evidence: if accelerator {
            "external-vendor-profiler-review-required"
        } else {
            "canonical-QDQ-graph-audit-and-ORT-CPU-reference"
        },
        exact_vector_conformance: exact,
        producer_eligible_without_external_review: exact && !accelerator && admitted_cpu_runtime,
        known_answers: results,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn build_model(
    bundle: VerifiedBundle,
    provider: &'static str,
    dispatch: ExecutionProviderDispatch,
    fixed_bucket: Option<usize>,
) -> anyhow::Result<TextEmbedding> {
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: bundle.tokenizer,
        config_file: bundle.model_config,
        special_tokens_map_file: bundle.special_tokens_map,
        tokenizer_config_file: bundle.tokenizer_config,
    };
    let mut user_model = UserDefinedEmbeddingModel::new(bundle.model, tokenizer_files)
        // The exported SentenceTransformers graph owns L2 normalization.
        // A second library-side reduction would be another floating-point
        // pipeline step outside the frozen artifact.
        .with_output_normalization(false);
    // The exported graph owns pooling, projection and L2 normalization.
    // Selecting this named 2-D output prevents FastEmbed from guessing a
    // token-level output and applying a second, backend-dependent pool.
    user_model.output_key = Some(OutputKey::ByName("sentence_embedding"));
    let mut options = InitOptionsUserDefined::new()
        .with_max_length(crate::embedding_profile::MAX_TOKENS)
        .with_intra_threads(crate::embedding_profile::ORT_INTRA_THREADS)
        // A registered EP is not proof that it owns the graph. Accelerator
        // packages fail session creation if ORT would place even one node on
        // its default CPU fallback.
        .with_disable_cpu_fallback(provider != "cpu")
        .with_execution_providers(vec![dispatch]);
    if let Some(bucket) = fixed_bucket {
        options = options
            .with_dimension_override("batch_size", 1)
            .with_dimension_override("sequence_length", bucket as i64);
    }
    TextEmbedding::try_new_from_user_defined(user_model, options)
        .with_context(|| format!("initialize verified model with ORT {provider} provider"))
}

fn set_fixed_padding(model: &mut TextEmbedding, bucket: usize) -> anyhow::Result<()> {
    let mut padding = model
        .tokenizer
        .get_padding()
        .cloned()
        .context("verified tokenizer has no padding configuration")?;
    padding.strategy = PaddingStrategy::Fixed(bucket);
    model.tokenizer.with_padding(Some(padding));
    Ok(())
}

fn execution_provider(
    requested: &str,
    _fixed_bucket: Option<usize>,
) -> anyhow::Result<(&'static str, &'static str, ExecutionProviderDispatch)> {
    let requested = requested.trim().to_ascii_lowercase();
    let requested = requested.as_str();

    // `auto` is a package property, not runtime roulette. Accelerator release
    // variants compile one of the provider features below; the generic ORT
    // package compiles CPU. Explicit names power conformance runs.
    if requested == "auto" {
        #[cfg(feature = "inference-qnn")]
        return qnn_provider();
        #[cfg(all(not(feature = "inference-qnn"), feature = "inference-vitis"))]
        return vitis_provider(_fixed_bucket);
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            feature = "inference-openvino"
        ))]
        return Ok(openvino_provider("openvino-npu"));
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            feature = "inference-coreml"
        ))]
        return Ok(coreml_provider("coreml-npu"));
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            feature = "inference-tensorrt"
        ))]
        return Ok(tensorrt_provider());
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            not(feature = "inference-tensorrt"),
            feature = "inference-cuda"
        ))]
        return Ok(cuda_provider());
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            not(feature = "inference-tensorrt"),
            not(feature = "inference-cuda"),
            feature = "inference-migraphx"
        ))]
        return Ok(migraphx_provider());
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            not(feature = "inference-tensorrt"),
            not(feature = "inference-cuda"),
            not(feature = "inference-migraphx"),
            feature = "inference-rocm"
        ))]
        return Ok(rocm_provider());
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            not(feature = "inference-tensorrt"),
            not(feature = "inference-cuda"),
            not(feature = "inference-migraphx"),
            not(feature = "inference-rocm"),
            feature = "inference-directml"
        ))]
        return Ok(directml_provider());
        #[cfg(all(
            not(feature = "inference-qnn"),
            not(feature = "inference-vitis"),
            not(feature = "inference-openvino"),
            not(feature = "inference-coreml"),
            not(feature = "inference-tensorrt"),
            not(feature = "inference-cuda"),
            not(feature = "inference-migraphx"),
            not(feature = "inference-rocm"),
            not(feature = "inference-directml"),
            feature = "inference-webgpu"
        ))]
        return Ok(webgpu_provider());
        #[cfg(not(any(
            feature = "inference-qnn",
            feature = "inference-vitis",
            feature = "inference-openvino",
            feature = "inference-coreml",
            feature = "inference-tensorrt",
            feature = "inference-cuda",
            feature = "inference-migraphx",
            feature = "inference-rocm",
            feature = "inference-directml",
            feature = "inference-webgpu"
        )))]
        return Ok(cpu_provider());
    }

    match requested {
        "cpu" => Ok(cpu_provider()),
        #[cfg(feature = "inference-coreml")]
        "coreml" | "coreml-npu" | "coreml-gpu" | "coreml-cpu" => Ok(coreml_provider(requested)),
        #[cfg(feature = "inference-openvino")]
        "openvino-npu" | "openvino-gpu" | "openvino-cpu" => Ok(openvino_provider(requested)),
        #[cfg(feature = "inference-qnn")]
        "qnn" => qnn_provider(),
        #[cfg(feature = "inference-vitis")]
        "vitis" => vitis_provider(_fixed_bucket),
        #[cfg(feature = "inference-cuda")]
        "cuda" => Ok(cuda_provider()),
        #[cfg(feature = "inference-tensorrt")]
        "tensorrt" => Ok(tensorrt_provider()),
        #[cfg(feature = "inference-migraphx")]
        "migraphx" => Ok(migraphx_provider()),
        #[cfg(feature = "inference-rocm")]
        "rocm" => Ok(rocm_provider()),
        #[cfg(feature = "inference-directml")]
        "directml" => Ok(directml_provider()),
        #[cfg(feature = "inference-webgpu")]
        "webgpu" => Ok(webgpu_provider()),
        name => anyhow::bail!(
            "ORT execution provider {name:?} is not compiled into this cfetch package"
        ),
    }
}

fn cpu_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "cpu",
        "cpu",
        ort::ep::CPU::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-coreml")]
fn coreml_provider(name: &str) -> (&'static str, &'static str, ExecutionProviderDispatch) {
    use ort::ep::coreml::{ComputeUnits, ModelFormat};

    let (name, units, class) = match name {
        "coreml-gpu" => ("coreml-gpu", ComputeUnits::CPUAndGPU, "gpu"),
        "coreml-cpu" => ("coreml-cpu", ComputeUnits::CPUOnly, "cpu"),
        "coreml" => ("coreml", ComputeUnits::All, "accelerator"),
        _ => ("coreml-npu", ComputeUnits::CPUAndNeuralEngine, "npu"),
    };
    (
        name,
        class,
        ort::ep::CoreML::default()
            .with_compute_units(units)
            .with_model_format(ModelFormat::MLProgram)
            .with_static_input_shapes(true)
            // The log is part of the external placement-review evidence. It
            // does not by itself grant producer admission.
            .with_profile_compute_plan(true)
            .with_low_precision_accumulation_on_gpu(false)
            .build()
            .error_on_failure(),
    )
}

#[cfg(feature = "inference-openvino")]
fn openvino_provider(name: &str) -> (&'static str, &'static str, ExecutionProviderDispatch) {
    let (name, device, class) = match name {
        "openvino-gpu" => ("openvino-gpu", "GPU", "gpu"),
        "openvino-cpu" => ("openvino-cpu", "CPU", "cpu"),
        _ => ("openvino-npu", "NPU", "npu"),
    };
    (
        name,
        class,
        ort::ep::OpenVINO::default()
            .with_device_type(device)
            .with_num_threads(crate::embedding_profile::ORT_INTRA_THREADS)
            .with_num_streams(1)
            .with_dynamic_shapes(false)
            .build()
            .error_on_failure(),
    )
}

#[cfg(feature = "inference-qnn")]
fn qnn_provider() -> anyhow::Result<(&'static str, &'static str, ExecutionProviderDispatch)> {
    let backend = std::env::var("CFETCH_QNN_HTP_LIBRARY").unwrap_or_else(|_| {
        if cfg!(windows) {
            "QnnHtp.dll".to_string()
        } else {
            "libQnnHtp.so".to_string()
        }
    });
    Ok((
        "qnn",
        "npu",
        ort::ep::QNN::default()
            .with_backend_path(backend)
            .build()
            .error_on_failure(),
    ))
}

#[cfg(feature = "inference-vitis")]
fn vitis_provider(
    fixed_bucket: Option<usize>,
) -> anyhow::Result<(&'static str, &'static str, ExecutionProviderDispatch)> {
    let target = std::env::var("CFETCH_VITIS_TARGET").unwrap_or_else(|_| "X2".to_string());
    anyhow::ensure!(
        matches!(target.as_str(), "X1" | "X2"),
        "CFETCH_VITIS_TARGET must be X1 or X2"
    );
    let xclbin = std::env::var("CFETCH_VITIS_XCLBIN")
        .ok()
        .filter(|path| !path.trim().is_empty());
    anyhow::ensure!(
        target != "X1" || xclbin.is_some(),
        "Ryzen AI X1 targets require CFETCH_VITIS_XCLBIN"
    );
    anyhow::ensure!(
        target != "X2" || xclbin.is_none(),
        "Ryzen AI X2 targets must not set CFETCH_VITIS_XCLBIN"
    );

    let mut provider = ort::ep::Vitis::default()
        .with_arbitrary_config("target", target)
        .with_arbitrary_config("opt_level", "0");
    if let Some(xclbin) = xclbin {
        provider = provider.with_arbitrary_config("xclbin", xclbin);
    }
    if let Some(report_dir) = std::env::var("CFETCH_VITIS_REPORT_DIR")
        .ok()
        .filter(|path| !path.trim().is_empty())
    {
        let bucket = fixed_bucket
            .context("CFETCH_VITIS_REPORT_DIR requires a fixed accelerator sequence bucket")?;
        provider = provider
            .with_cache_dir(report_dir)
            .with_cache_key(format!("cfetch-v1-seq-{bucket}"))
            // AMD requires disk-backed compilation for its operator-assignment
            // report. Bucket-specific cache keys prevent one compiled static
            // shape from being reused for another.
            .with_arbitrary_config("enable_cache_file_io_in_mem", "0")
            .with_arbitrary_config("ai_analyzer_profiling", "true");
    }

    Ok(("vitis", "npu", provider.build().error_on_failure()))
}

#[cfg(feature = "inference-cuda")]
fn cuda_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "cuda",
        "gpu",
        ort::ep::CUDA::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-tensorrt")]
fn tensorrt_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "tensorrt",
        "gpu",
        ort::ep::TensorRT::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-migraphx")]
fn migraphx_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    // Do not call `with_int8(true)`: that asks MIGraphX to calibrate and
    // quantize the input graph again. v1 supplies its immutable Q/DQ scales.
    (
        "migraphx",
        "gpu",
        ort::ep::MIGraphX::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-rocm")]
fn rocm_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "rocm",
        "gpu",
        ort::ep::ROCm::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-directml")]
fn directml_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "directml",
        "gpu",
        ort::ep::DirectML::default().build().error_on_failure(),
    )
}

#[cfg(feature = "inference-webgpu")]
fn webgpu_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "webgpu",
        "gpu",
        ort::ep::WebGPU::default().build().error_on_failure(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_ort_package_selects_cpu() {
        #[cfg(not(any(
            feature = "inference-qnn",
            feature = "inference-vitis",
            feature = "inference-openvino",
            feature = "inference-coreml",
            feature = "inference-tensorrt",
            feature = "inference-cuda",
            feature = "inference-migraphx",
            feature = "inference-rocm",
            feature = "inference-directml",
            feature = "inference-webgpu"
        )))]
        {
            let (name, class, _) = execution_provider("auto", None).unwrap();
            assert_eq!((name, class), ("cpu", "cpu"));
        }
    }

    #[test]
    fn unavailable_provider_is_never_silently_cpu() {
        let error = execution_provider("definitely-not-compiled", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not compiled"), "{error}");
    }

    #[test]
    fn production_requires_exact_bytes_and_a_runtime_admission() {
        ensure_production_admission("cpu", true, true).unwrap();

        let drift = ensure_production_admission("cpu", false, false)
            .unwrap_err()
            .to_string();
        assert!(drift.contains("incompatible") && drift.contains("local production is disabled"));

        let unreviewed = ensure_production_admission("qnn", true, false)
            .unwrap_err()
            .to_string();
        assert!(unreviewed.contains("no production admission"));
    }
}
