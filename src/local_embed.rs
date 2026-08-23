//! Verified local embeddings through FastEmbed's tokenizer/pipeline and ONNX
//! Runtime's execution providers.
//!
//! This module is compiled only into inference variants. It never downloads a
//! model: [`crate::model_artifact::VerifiedBundle`] admits the exact v1 bytes
//! before ORT sees them.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;
use fastembed::{
    InitOptionsUserDefined, OutputKey, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use ort::ep::ExecutionProviderDispatch;
use tokenizers::{PaddingStrategy, TruncationParams};

use crate::model_artifact::VerifiedBundle;

pub struct LocalEmbedder {
    provider: &'static str,
    device_class: &'static str,
    length_tokenizer: tokenizers::Tokenizer,
    model: Mutex<ModelState>,
}

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
    pub fn load(model_dir: &Path, requested_provider: &str) -> anyhow::Result<Self> {
        let bundle = VerifiedBundle::load(model_dir)?;
        let (provider, device_class, dispatch) = execution_provider(requested_provider)?;
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
                        let (_, _, dispatch) = execution_provider(self.provider)?;
                        *current = Some((
                            bucket,
                            build_model(bundle.clone(), self.provider, dispatch, Some(bucket))?,
                        ));
                    }
                    &mut current.as_mut().expect("accelerator session initialized").1
                }
            };
            set_fixed_padding(model, bucket)?;
            let mut vector = model
                .embed([*text], Some(crate::embedding_profile::INFERENCE_BATCH_SIZE))
                .with_context(|| {
                    format!(
                        "run verified model with ORT {} provider at sequence bucket {bucket}",
                        self.provider
                    )
                })?;
            outputs.push(
                vector
                    .pop()
                    .context("verified local model returned no embedding")?,
            );
        }
        Ok(outputs)
    }
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
    let mut user_model = UserDefinedEmbeddingModel::new(bundle.model, tokenizer_files);
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
        return Ok(vitis_provider());
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
        "vitis" => Ok(vitis_provider()),
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
    use ort::ep::coreml::ComputeUnits;

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
fn vitis_provider() -> (&'static str, &'static str, ExecutionProviderDispatch) {
    (
        "vitis",
        "npu",
        ort::ep::Vitis::default().build().error_on_failure(),
    )
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
            let (name, class, _) = execution_provider("auto").unwrap();
            assert_eq!((name, class), ("cpu", "cpu"));
        }
    }

    #[test]
    fn unavailable_provider_is_never_silently_cpu() {
        let error = execution_provider("definitely-not-compiled")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not compiled"), "{error}");
    }
}
