// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "CFetchAppleCoreMLCandidate",
    platforms: [
        .macOS(.v15),
    ],
    dependencies: [
        .package(
            url: "https://github.com/john-rocky/CoreML-LLM.git",
            revision: "5ef6b301d3a3d628e25c0605479f59dbf3a7d955"
        ),
    ],
    targets: [
        .executableTarget(
            name: "AppleCoreMLCandidate",
            dependencies: [
                .product(name: "CoreMLLLM", package: "CoreML-LLM"),
            ],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
