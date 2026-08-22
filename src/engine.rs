//! Which inference engine drives which accelerator, and which one this build
//! actually contains.
//!
//! cfetch ships a binary per accelerator rather than one binary that carries
//! every runtime, because the runtimes do not overlap: Core ML is Apple-only,
//! LiteRT's NPU delegates are per-vendor, OpenVINO is Intel's. A single fat
//! binary would be most of a gigabyte to give every user code they cannot run.
//!
//! So the variant is chosen at BUILD time (which engines are compiled in) and
//! the device at RUN time (what the machine has). This module is where those
//! two facts meet. It deliberately holds no inference code — it is the socket,
//! and each engine is a plug.
//!
//! The format column is not incidental. Between formats we choose by
//! efficiency and performance, never by what would be convenient to maintain,
//! which is why this is a table of native runtimes rather than one portable
//! one. ONNX Runtime looked like it could be the universal answer and is not:
//! against a real EmbeddingGemma graph its Core ML provider leaves 23.6% of
//! nodes unsupported, so Apple gets native Core ML instead.

use crate::hardware::{Device, Found};

/// An inference backend cfetch can be built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Any OpenAI-compatible `/embeddings` endpoint over HTTP. Always
    /// available, drives no local accelerator, and is what every build can
    /// fall back to — including a host that holds nothing and asks its
    /// serving host instead.
    Endpoint,
    /// ONNX Runtime. Excellent on NVIDIA and CPU; the stock EmbeddingGemma
    /// export runs unmodified with zero unsupported nodes.
    OnnxRuntime,
    /// llama.cpp as a library. GGUF, and the strongest CPU kernels.
    LlamaCpp,
    /// Apple Core ML. The only path to the Neural Engine — 99.80% of the
    /// model's ops dispatch there, against 76.4% through ONNX Runtime.
    CoreMl,
    /// Google LiteRT. Reaches Qualcomm and MediaTek NPUs through vendor
    /// delegates, with per-SoC artifacts compiled ahead of time.
    LiteRt,
    /// Intel OpenVINO. The Intel NPU's own runtime; ONNX Runtime's OpenVINO
    /// provider publishes no NPU operator table at all.
    OpenVino,
}

impl Backend {
    /// Every backend, in the order a build should prefer them for a device
    /// they both support.
    pub const ALL: &'static [Backend] = &[
        Backend::CoreMl,
        Backend::LiteRt,
        Backend::OpenVino,
        Backend::OnnxRuntime,
        Backend::LlamaCpp,
        Backend::Endpoint,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Endpoint => "endpoint",
            Backend::OnnxRuntime => "onnxruntime",
            Backend::LlamaCpp => "llama.cpp",
            Backend::CoreMl => "coreml",
            Backend::LiteRt => "litert",
            Backend::OpenVino => "openvino",
        }
    }

    /// The model format this backend consumes.
    pub fn format(self) -> &'static str {
        match self {
            Backend::Endpoint => "none (remote)",
            Backend::OnnxRuntime => "onnx",
            Backend::LlamaCpp => "gguf",
            Backend::CoreMl => "mlpackage",
            Backend::LiteRt => "tflite",
            Backend::OpenVino => "openvino-ir",
        }
    }

    /// Whether THIS build contains the backend. Each is a Cargo feature, so a
    /// variant is a feature selection rather than a source fork.
    pub fn compiled_in(self) -> bool {
        match self {
            // Always: it is plain HTTP and it is the fallback everything else
            // degrades to.
            Backend::Endpoint => true,
            Backend::OnnxRuntime => cfg!(feature = "onnx"),
            Backend::LlamaCpp => cfg!(feature = "llamacpp"),
            Backend::CoreMl => cfg!(feature = "coreml"),
            Backend::LiteRt => cfg!(feature = "litert"),
            Backend::OpenVino => cfg!(feature = "openvino"),
        }
    }

    /// Whether this backend can drive this device. The rows are the PRD's
    /// per-platform format table, as code.
    pub fn drives(self, device: Device) -> bool {
        match (self, device) {
            // Core ML is the ONLY route to the Neural Engine.
            (Backend::CoreMl, Device::AppleNeuralEngine | Device::AppleGpu) => true,
            // LiteRT reaches Qualcomm through the vendor delegate.
            (Backend::LiteRt, Device::QualcommNpu) => true,
            // OpenVINO is the Intel NPU's own runtime, and also drives Intel
            // GPUs and CPUs.
            (Backend::OpenVino, Device::IntelNpu | Device::IntelGpu | Device::Cpu) => true,
            // ONNX Runtime: NVIDIA and CPU are where it is unambiguously
            // strong. It is NOT listed for any NPU — its coverage there is
            // either unsupported (Core ML) or undocumented (OpenVINO NPU).
            (Backend::OnnxRuntime, Device::NvidiaGpu | Device::Cpu) => true,
            // llama.cpp drives GPUs through its own backends and has the
            // strongest CPU kernels. It reaches no NPU: the NPU backends it
            // does have are validated for LLMs, and its own table marks the
            // one embedding model it lists as unsupported there.
            (
                Backend::LlamaCpp,
                Device::NvidiaGpu | Device::AmdGpu | Device::IntelGpu | Device::AppleGpu | Device::Cpu,
            ) => true,
            // The endpoint drives nothing locally — that is the point of it.
            _ => false,
        }
    }
}

/// What this build will actually use on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub device: Device,
    pub backend: Backend,
}

/// Picks the best usable device this build can actually drive.
///
/// `found` must be ordered best-first, which `hardware::detect` guarantees.
/// A device nothing compiled in can drive is skipped rather than selected and
/// then failed on — so a CPU-only build on a machine full of accelerators
/// correctly answers "CPU", not "NPU, and then a crash".
///
/// Falls back to the remote endpoint, which is always available and is the
/// right answer for a host that holds nothing.
pub fn select(found: &[Found]) -> Selection {
    for f in found.iter().filter(|f| f.usable().is_ok()) {
        for b in Backend::ALL.iter().copied().filter(|b| b.compiled_in()) {
            if b.drives(f.device) {
                return Selection { device: f.device, backend: b };
            }
        }
    }
    Selection { device: Device::Cpu, backend: Backend::Endpoint }
}

/// The backends this build contains, for reporting.
pub fn compiled_backends() -> Vec<Backend> {
    Backend::ALL.iter().copied().filter(|b| b.compiled_in()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(device: Device) -> Found {
        Found { device, evidence: "test".into(), pci_device: None }
    }

    #[test]
    fn a_build_with_no_engines_answers_endpoint_however_good_the_hardware() {
        // The default build links no inference at all; a machine full of
        // accelerators must still get a usable answer rather than a promise
        // this binary cannot keep.
        let sel = select(&[f(Device::AppleNeuralEngine), f(Device::NvidiaGpu), f(Device::Cpu)]);
        if !Backend::OnnxRuntime.compiled_in() && !Backend::CoreMl.compiled_in() {
            assert_eq!(sel.backend, Backend::Endpoint);
        }
    }

    #[test]
    fn the_endpoint_is_always_available_and_drives_nothing_local() {
        assert!(Backend::Endpoint.compiled_in());
        for d in [
            Device::IntelNpu,
            Device::AmdNpu,
            Device::QualcommNpu,
            Device::AppleNeuralEngine,
            Device::NvidiaGpu,
            Device::AmdGpu,
            Device::IntelGpu,
            Device::AppleGpu,
            Device::Cpu,
        ] {
            assert!(!Backend::Endpoint.drives(d), "{d:?}");
        }
    }

    #[test]
    fn only_core_ml_reaches_the_neural_engine() {
        for b in Backend::ALL {
            assert_eq!(
                b.drives(Device::AppleNeuralEngine),
                *b == Backend::CoreMl,
                "{b:?} and the ANE"
            );
        }
    }

    #[test]
    fn no_backend_claims_to_drive_an_npu_it_cannot() {
        // llama.cpp and ONNX Runtime both LOOK like they should reach NPUs
        // and neither does for an encoder. Encoding that stops a future
        // variant from being built on a false premise.
        for npu in [Device::IntelNpu, Device::AmdNpu, Device::QualcommNpu, Device::AppleNeuralEngine]
        {
            assert!(!Backend::LlamaCpp.drives(npu), "llama.cpp reaches no NPU: {npu:?}");
            assert!(!Backend::OnnxRuntime.drives(npu), "ORT is not listed for NPUs: {npu:?}");
        }
        // Except the two that genuinely do, each through its vendor runtime.
        assert!(Backend::OpenVino.drives(Device::IntelNpu));
        assert!(Backend::LiteRt.drives(Device::QualcommNpu));
    }

    #[test]
    fn every_backend_names_the_format_it_consumes() {
        for b in Backend::ALL {
            assert!(!b.name().is_empty());
            assert!(!b.format().is_empty(), "{b:?}");
        }
        assert_eq!(Backend::CoreMl.format(), "mlpackage");
        assert_eq!(Backend::LiteRt.format(), "tflite");
        assert_eq!(Backend::OpenVino.format(), "openvino-ir");
    }

    #[test]
    fn an_unusable_device_is_never_selected() {
        // The AMD NPU exists and cannot run the model class; selection must
        // walk past it to the GPU rather than choose it and fail later.
        let sel = select(&[f(Device::AmdNpu), f(Device::AmdGpu), f(Device::Cpu)]);
        assert_ne!(sel.device, Device::AmdNpu);
    }
}
