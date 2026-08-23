import CoreML
import Foundation
import Metal

struct Device: Codable {
    let kind: String
    let description: String
}

struct MetalDevice: Codable {
    let name: String
    let registryID: UInt64
    let lowPower: Bool
    let removable: Bool
    let unifiedMemory: Bool
}

struct Report: Codable {
    let schema: Int
    let architecture: String
    let operatingSystem: String
    let coreMLDevices: [Device]
    let metalDevices: [MetalDevice]
    let neuralEngineAccessible: Bool
    let metalAccessible: Bool
}

func deviceKind(_ device: MLComputeDevice) -> String {
    switch device {
    case .cpu:
        return "cpu"
    case .gpu:
        return "gpu"
    case .neuralEngine:
        return "neural-engine"
    @unknown default:
        return "unknown"
    }
}

let coreMLDevices = MLComputeDevice.allComputeDevices.map {
    Device(kind: deviceKind($0), description: String(describing: $0))
}
let metalDevices = MTLCopyAllDevices().map {
    MetalDevice(
        name: $0.name,
        registryID: $0.registryID,
        lowPower: $0.isLowPower,
        removable: $0.isRemovable,
        unifiedMemory: $0.hasUnifiedMemory
    )
}
let process = ProcessInfo.processInfo
let report = Report(
    schema: 1,
    architecture: process.machineArchitecture,
    operatingSystem: process.operatingSystemVersionString,
    coreMLDevices: coreMLDevices,
    metalDevices: metalDevices,
    neuralEngineAccessible: coreMLDevices.contains { $0.kind == "neural-engine" },
    metalAccessible: !metalDevices.isEmpty
)

let encoder = JSONEncoder()
encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
FileHandle.standardOutput.write(try encoder.encode(report))
FileHandle.standardOutput.write(Data("\n".utf8))

extension ProcessInfo {
    var machineArchitecture: String {
        var systemInfo = utsname()
        uname(&systemInfo)
        return withUnsafePointer(to: &systemInfo.machine) {
            $0.withMemoryRebound(to: CChar.self, capacity: 1) {
                String(cString: $0)
            }
        }
    }
}
