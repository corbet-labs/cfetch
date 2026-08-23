# Accelerator certification

cfetch network major 1 has one embedding profile and one `INT8x768` vector
record. Hardware backends are alternative runners for that profile, not
alternative models or quantizers. A backend may publish vectors only after it
passes the canonical known-answer test byte for byte with the exact released
model artifact.

There are three distinct levels of evidence:

1. **Discovery** proves that an operating system exposes a device and runtime.
2. **Execution** proves that the released model actually runs on that device,
   without silently falling back to CPU.
3. **Conformance** proves that every known-answer input produces the exact
   released 768-byte vector and records the artifact, runtime, driver, device,
   operating system, provider placement, and timing.

Only level 3 admits a producer backend. Discovery or successful graph loading
alone never changes the release catalog.

## Public CI

`.github/workflows/accelerator-discovery.yml` runs an Apple probe on the
standard `macos-15` Apple-silicon runner and the standard
`macos-15-intel` runner. The workflow uses no secrets and uploads no model or
artifact. Its public log reports the Core ML compute devices and Metal devices
that the hosted virtual machine can actually access.

GitHub's public standard runner is useful only when the required accelerator
is exposed to the virtual machine. A runner whose CPU architecture is Apple
silicon does not, by itself, prove that Metal or the Neural Engine is exposed.
The final Core ML package must still be run and its compute plan inspected.

## Physical tester matrix

The release matrix requires physical-device reports for accelerator classes
that public hosted CI does not expose. Reports are reusable only for the exact
artifact digest and runtime/driver combination they name.

| Backend | Minimum physical evidence |
|---|---|
| CPU | x86-64 legacy, v3, v4, and arm64 reference runs |
| Apple | Apple silicon generations supported by the package; ANE and Metal placement separately |
| Intel | OpenVINO CPU, GPU, and NPU with per-node placement/fallback evidence |
| AMD GPU | ROCm where supported and the portable fallback on an older consumer GPU |
| AMD NPU | Physical XDNA2 execution through Ryzen AI; conversion on a CPU is not evidence |
| NVIDIA | CUDA and TensorRT on both the oldest supported and current architecture |
| Qualcomm | Windows arm64 QNN HTP execution on a Snapdragon X-class device |

A failed backend remains useful as a metadata or none-tier participant: it can
consume the shared vectors or ask a certified producer to derive them. It must
not advertise an embed capability.

## Report contract

The permanent conformance command will emit one JSON object suitable for a
public issue or CI log. It must contain no usernames, hostnames, private paths,
environment variables, model inputs beyond the public known-answer corpus, or
other deployment context. The required fields are:

- cfetch version, network major, profile ID, and manifest digest;
- canonical artifact digest and native derived-container digest;
- backend, execution provider, provider/runtime version, and fallback policy;
- device name and stable hardware identifier where the operating system makes
  one public;
- operating system, architecture, driver version, and fixed sequence bucket;
- provider placement/coverage evidence;
- every known-answer vector digest and exact pass/fail result;
- cold-load, warm-load, latency, throughput, and peak-memory measurements.

The release catalog is generated only from reviewed passing reports. A local
self-attestation cannot turn an unknown binary into a producer.
