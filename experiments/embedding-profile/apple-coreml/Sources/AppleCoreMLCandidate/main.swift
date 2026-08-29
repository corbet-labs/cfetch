import CoreML
import CoreMLLLM
import CryptoKit
import Foundation

private let coreMLLLMRevision = "5ef6b301d3a3d628e25c0605479f59dbf3a7d955"
private let artifactRepository = "valindotai/embeddinggemma-300m-coreml"
private let artifactRevision = "d1dc305086782e958f91fa278de97e4af9caeaf0"
private let artifactWeightSHA256 = "62e84aaaa99bc7950668742301eaadb0b1a23204b5b9204dfaf20bfdd02bdf9d"
private let targetModel = "google/embeddinggemma-300m"
private let targetModelRevision = "57c266a740f537b4dc058e1b0cda161fd15afa75"
private let targetProfileManifestSHA256 = "59210a333494f788eb8e607fe38cabb6af1a7aa7cdf604ddf52e3fa6004b5afb"

// These are already prefixed exactly as cfetch sends them to an embedding
// adapter. Passing task: nil below is deliberate: adding a second prefix would
// put this candidate in a different semantic space.
private let queryText =
    "task: search result | query: Which planet is known as the Red Planet?"
private let documentText =
    "title: none | text: Mars is known as the Red Planet because iron minerals in its soil oxidize."

private enum SmokeError: Error, CustomStringConvertible {
    case usage
    case unsupportedArchitecture(String)
    case unexpectedModelConfig(maxSequenceLength: Int, dimensions: Int)
    case invalidVector(role: String, reason: String)
    case indistinguishableRoles
    case nonRepeatable(role: String)
    case codecSelfTest

    var description: String {
        switch self {
        case .usage:
            return "usage: AppleCoreMLCandidate --bundle <embeddinggemma bundle>"
        case .unsupportedArchitecture(let architecture):
            return "this candidate must run on arm64, not \(architecture)"
        case .unexpectedModelConfig(let maxSequenceLength, let dimensions):
            return "expected the fixed-seq256 768d artifact, got seq\(maxSequenceLength) \(dimensions)d"
        case .invalidVector(let role, let reason):
            return "\(role) output is invalid: \(reason)"
        case .indistinguishableRoles:
            return "query and document produced the same canonical INT8 output"
        case .nonRepeatable(let role):
            return "\(role) canonical INT8 output changed on the immediate repeat"
        case .codecSelfTest:
            return "canonical INT8 maxabs/RNE codec self-test failed"
        }
    }
}

private struct VectorEvidence {
    let role: String
    let maxAbsolute: Float
    let l2Norm: Double
    let quantized: [Int8]

    var json: [String: Any] {
        let bytes = quantized.map { UInt8(bitPattern: $0) }
        let digest = SHA256.hash(data: Data(bytes))
            .map { String(format: "%02x", $0) }
            .joined()
        return [
            "dimensions": quantized.count,
            "finite": true,
            "int8_max": Int(quantized.max() ?? 0),
            "int8_min": Int(quantized.min() ?? 0),
            "int8_nonzero": quantized.lazy.filter { $0 != 0 }.count,
            "int8_sha256": digest,
            "l2_norm": l2Norm,
            "max_absolute": Double(maxAbsolute),
            "role": role,
        ]
    }
}

/// cfetch's scale-free signed INT8 codec: the maximum absolute component maps
/// to 127, other components use round-to-nearest-even, and -128 is unused.
private func canonicalInt8(_ vector: [Float]) throws -> [Int8] {
    guard vector.allSatisfy(\.isFinite) else {
        throw SmokeError.invalidVector(role: "codec", reason: "contains a non-finite component")
    }
    let maximum = vector.reduce(Float.zero) { max($0, abs($1)) }
    guard maximum > 0 else {
        throw SmokeError.invalidVector(role: "codec", reason: "all components are zero")
    }
    return vector.map { component in
        let rounded = (component / maximum * 127).rounded(.toNearestOrEven)
        let clamped = min(Float(127), max(Float(-127), rounded))
        return Int8(Int(clamped))
    }
}

private func inspect(role: String, vector: [Float]) throws -> VectorEvidence {
    guard vector.count == 768 else {
        throw SmokeError.invalidVector(role: role, reason: "expected 768 components, got \(vector.count)")
    }
    guard vector.allSatisfy(\.isFinite) else {
        throw SmokeError.invalidVector(role: role, reason: "contains a non-finite component")
    }
    let maximum = vector.reduce(Float.zero) { max($0, abs($1)) }
    guard maximum > 0 else {
        throw SmokeError.invalidVector(role: role, reason: "all components are zero")
    }
    let norm = vector.reduce(0.0) { partial, component in
        partial + Double(component) * Double(component)
    }.squareRoot()
    guard abs(norm - 1.0) <= 0.02 else {
        throw SmokeError.invalidVector(role: role, reason: "expected L2 unit norm, got \(norm)")
    }
    return VectorEvidence(
        role: role,
        maxAbsolute: maximum,
        l2Norm: norm,
        quantized: try canonicalInt8(vector)
    )
}

private func bundleArgument() throws -> URL {
    let arguments = Array(CommandLine.arguments.dropFirst())
    guard arguments.count == 2, arguments[0] == "--bundle" else {
        throw SmokeError.usage
    }
    return URL(fileURLWithPath: arguments[1], isDirectory: true)
}

@main
private struct AppleCoreMLCandidate {
    static func main() async throws {
        #if arch(arm64)
        let architecture = "arm64"
        #elseif arch(x86_64)
        let architecture = "x86_64"
        #else
        let architecture = "unknown"
        #endif
        guard architecture == "arm64" else {
            throw SmokeError.unsupportedArchitecture(architecture)
        }

        // Lock down ties-to-even independently of the model. Float arithmetic
        // is intentional here because cfetch's Rust codec operates on f32.
        let codecFixture: [Float] = [
            1, -1,
            Float(0.5) / 127, Float(1.5) / 127,
            Float(-0.5) / 127, Float(-1.5) / 127,
        ]
        guard try canonicalInt8(codecFixture) == [127, -127, 0, 2, 0, -2] else {
            throw SmokeError.codecSelfTest
        }

        let bundleURL = try bundleArgument()
        let embedder = try await EmbeddingGemma.load(
            bundleURL: bundleURL,
            computeUnits: .cpuAndNeuralEngine
        )
        guard embedder.config.maxSeqLen == 256, embedder.config.embedDim == 768 else {
            throw SmokeError.unexpectedModelConfig(
                maxSequenceLength: embedder.config.maxSeqLen,
                dimensions: embedder.config.embedDim
            )
        }

        let cases = [("query", queryText), ("document", documentText)]
        var vectorEvidence: [VectorEvidence] = []
        for (role, text) in cases {
            let first = try inspect(
                role: role,
                vector: embedder.encode(text: text, task: nil, dim: 768)
            )
            let repeatRun = try inspect(
                role: role,
                vector: embedder.encode(text: text, task: nil, dim: 768)
            )
            guard first.quantized == repeatRun.quantized else {
                throw SmokeError.nonRepeatable(role: role)
            }
            vectorEvidence.append(first)
        }
        guard vectorEvidence[0].quantized != vectorEvidence[1].quantized else {
            throw SmokeError.indistinguishableRoles
        }

        let evidence: [String: Any] = [
            "accelerator_placement_proven": false,
            "architecture": architecture,
            "artifact_lineage_to_target_revision_proven": false,
            "artifact_repository": artifactRepository,
            "artifact_revision": artifactRevision,
            "artifact_weight_sha256": artifactWeightSHA256,
            "candidate_only": true,
            "codec": "signed-int8x768-maxabs-rne",
            "codec_self_test": true,
            "coreml_llm_revision": coreMLLLMRevision,
            "full_profile_max_sequence_length": 2048,
            "global_all_pairs_admitted": false,
            "model_max_sequence_length": embedder.config.maxSeqLen,
            "profile_max_sequence_length_covered": false,
            "profile_id": "cfetch-embedding-v1",
            "requested_compute_units": "cpuAndNeuralEngine",
            "runtime_truncates_beyond_compiled_length": true,
            "schema_version": 1,
            "silent_truncation_is_admissible": false,
            "target_model": targetModel,
            "target_model_revision": targetModelRevision,
            "target_profile_manifest_sha256": targetProfileManifestSHA256,
            "vectors": vectorEvidence.map(\.json),
        ]
        let json = try JSONSerialization.data(withJSONObject: evidence, options: [.sortedKeys])
        FileHandle.standardOutput.write(json)
        FileHandle.standardOutput.write(Data([0x0A]))
    }
}
