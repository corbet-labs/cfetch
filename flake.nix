# Nix packaging for cfetch. The version is read from Cargo.toml so releases
# bump exactly one file; the build runs `cargo test` in its checkPhase, and
# `checks` reuses the package so `nix flake check` does not compile twice.
{
  description = "cfetch — a second brain for coding agents: privilege-ring memory, hook injection, retrieval, and a code index in one binary";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (import nixpkgs {
            inherit system;
            # FSL is source-available, so meta.license honestly carries
            # free = false — but without this predicate every consumer of this
            # flake would need NIXPKGS_ALLOW_UNFREE=1 just to install cfetch
            # from cfetch's own flake. Scoped to exactly this package.
            config.allowUnfreePredicate = pkg:
              let
                name = nixpkgs.lib.getName pkg;
              in
              nixpkgs.lib.hasPrefix "cuda_" name || builtins.elem name [
                "cfetch"
                "cfetch-local-cpu"
                "cfetch-test-coreml"
                "cfetch-test-cuda"
                "cfetch-test-migraphx"
                "cfetch-test-openvino"
                "cfetch-test-openvino-current"
                "cfetch-test-tensorrt"
                "cfetch-test-webgpu"
                "cfetch-cudnn-runtime"
                "cfetch-tensorrt-runtime"
                "libcublas"
                "libcurand"
              ];
            # Nixpkgs only builds ONNX Runtime's MIGraphX provider when the
            # ROCm package set is enabled. Keep that expensive closure scoped
            # to the one platform on which the certification package exists.
            config.rocmSupport = system == "x86_64-linux";
          })
        );
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs:
        let
        # Pin Microsoft's official ORT release bytes. Two builds that both
        # reported "1.28.0" produced different W8A8 vectors in the adverse
        # test; runtime provenance is therefore part of producer admission.
        ortVersion = "1.28.0";
        ortAsset = {
          x86_64-linux = {
            file = "onnxruntime-linux-x64-${ortVersion}.tgz";
            hash = "sha256-o+G3nXuxvwlpbOZ19J5AZObIH2ICuCJWJP/w6T+NZAc=";
            sha256 = "a3e1b79d7bb1bf09696ce675f49e4064e6c81f6202b8225624fff0e93f8d6407";
          };
          aarch64-linux = {
            file = "onnxruntime-linux-aarch64-${ortVersion}.tgz";
            hash = "sha256-4V/4tdha/mwUTZfG/UMiVL92ohnarxdlgIfW7LPo8Ls=";
            sha256 = "e15ff8b5d85afe6c144d97c6fd432254bf76a219daaf17658087d6ecb3e8f0bb";
          };
          aarch64-darwin = {
            file = "onnxruntime-osx-arm64-${ortVersion}.tgz";
            hash = "sha256-EmizWXGAmb3izttVeH8YKhMAZ7xPMejIhHjERbhQ09g=";
            sha256 = "1268b359718099bde2cedb55787f182a130067bc4f31e8c88478c445b850d3d8";
          };
        }.${pkgs.stdenv.hostPlatform.system};
        certifiedOrt = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-certified-onnxruntime";
          version = ortVersion;
          src = pkgs.fetchurl {
            url = "https://github.com/microsoft/onnxruntime/releases/download/v${ortVersion}/${ortAsset.file}";
            inherit (ortAsset) hash;
          };
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp -R include lib "$out/"
            install -Dm644 LICENSE "$out/share/licenses/onnxruntime/LICENSE"
            install -Dm644 ThirdPartyNotices.txt \
              "$out/share/licenses/onnxruntime/ThirdPartyNotices.txt"
            runHook postInstall
          '';
        };
        webgpuPluginVersion = "0.2.1";
        webgpuPluginArchiveSha256 = "a707557c86eb1eee0a604146ac4edc473d5af0bfe2fc77fd632217755cbfb282";
        webgpuAsset = {
          x86_64-linux = {
            runtime = "linux-x64";
            library = "libonnxruntime_providers_webgpu.so";
          };
          aarch64-darwin = {
            runtime = "osx-arm64";
            library = "libonnxruntime_providers_webgpu.dylib";
          };
        }.${pkgs.stdenv.hostPlatform.system};
        # Native WebGPU is an independently versioned plugin EP. Microsoft's
        # official NuGet carries the Vulkan, D3D12 and Metal libraries plus
        # the corresponding notices.
        webgpuPlugin = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-onnxruntime-webgpu-plugin";
          version = webgpuPluginVersion;
          src = pkgs.fetchurl {
            url = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime.ep.webgpu/${webgpuPluginVersion}/microsoft.ml.onnxruntime.ep.webgpu.${webgpuPluginVersion}.nupkg";
            hash = "sha256-pwdVfIbrHu4KYEFGrE7cRz1a8L/i/Hf9YyIXdVy/soI=";
          };
          dontUnpack = true;
          nativeBuildInputs = [ pkgs.unzip ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/lib" "$out/share/licenses/onnxruntime-webgpu"
            unzip -j "$src" \
              'runtimes/${webgpuAsset.runtime}/native/${webgpuAsset.library}' \
              -d "$out/lib"
            unzip -p "$src" LICENSE \
              > "$out/share/licenses/onnxruntime-webgpu/LICENSE"
            unzip -p "$src" ThirdPartyNotices.txt \
              > "$out/share/licenses/onnxruntime-webgpu/ThirdPartyNotices.txt"
            runHook postInstall
          '';
          meta.license = pkgs.lib.licenses.mit;
        };
        webgpuOrt = pkgs.symlinkJoin {
          name = "cfetch-webgpu-onnxruntime-${ortVersion}-${webgpuPluginVersion}";
          paths = [ certifiedOrt webgpuPlugin ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              (pkgs.lib.getLib pkgs.vulkan-loader)
            ];
        };
        openvinoVersion = "1.24.1";
        openvinoWheelSha256 = "2c3bb73e68ac27f4891af8a595c1faf574ec68b772e6583c90a0b997a1822782";
        openvinoOrt = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-openvino-onnxruntime";
          version = openvinoVersion;
          src = pkgs.fetchurl {
            url = "https://files.pythonhosted.org/packages/08/07/f225999919f56506b603aaa3ff837ad563ab26f86906ed7fa7e5abcd849e/onnxruntime_openvino-${openvinoVersion}-cp313-cp313-manylinux_2_28_x86_64.whl";
            hash = "sha256-LDu3PmisJ/SJGvillcH69XTsaLdy5lg8kKC5l6GCJ4I=";
          };
          dontUnpack = true;
          nativeBuildInputs = [ pkgs.unzip ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/lib" "$out/share/licenses/onnxruntime-openvino"
            unzip -j "$src" 'onnxruntime/capi/lib*.so*' -d "$out/lib"
            ln -s libonnxruntime.so.${openvinoVersion} "$out/lib/libonnxruntime.so"
            unzip -p "$src" onnxruntime/LICENSE \
              > "$out/share/licenses/onnxruntime-openvino/LICENSE"
            unzip -p "$src" onnxruntime/ThirdPartyNotices.txt \
              > "$out/share/licenses/onnxruntime-openvino/ThirdPartyNotices.txt"
            runHook postInstall
          '';
        };
        # Microsoft's CUDA 12 archive contains both CUDA and TensorRT EPs. It
        # is preferable to rebuilding ORT here: the exact vendor release bytes
        # are independently hashable certification inputs. Nix supplies only
        # the ABI-compatible redistributables that the archive intentionally
        # leaves external; the physical host still supplies libcuda.
        nvidiaOrt = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-nvidia-onnxruntime";
          version = ortVersion;
          src = pkgs.fetchurl {
            url = "https://github.com/microsoft/onnxruntime/releases/download/v${ortVersion}/onnxruntime-linux-x64-gpu_cuda12-${ortVersion}.tgz";
            hash = "sha256-6mvStl19+rvrksSvXdjxLlrthgHlRK03jS+HInVDixo=";
          };
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp -R include lib "$out/"
            install -Dm644 LICENSE "$out/share/licenses/onnxruntime/LICENSE"
            install -Dm644 ThirdPartyNotices.txt \
              "$out/share/licenses/onnxruntime/ThirdPartyNotices.txt"
            runHook postInstall
          '';
        };
        # Runtime-only wheels avoid nixpkgs' 8.55 GiB TensorRT SDK download.
        # They are still large because TensorRT ships its device kernels, but
        # they contain exactly the libraries ORT needs and retain NVIDIA's
        # non-redistributable licenses as separate, tester-local Nix inputs.
        nvidiaCudnn = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-cudnn-runtime";
          version = "9.22.0.52";
          src = pkgs.fetchurl {
            url = "https://files.pythonhosted.org/packages/a0/8f/2ede6b758b7524608472010f632bdd3370ea271d715d1d66044614b84cdc/nvidia_cudnn_cu12-9.22.0.52-py3-none-manylinux_2_27_x86_64.whl";
            hash = "sha256-ORuafuY4barKf43KQeg8LJn3YMlYGgQAdV6HtCh7iEc=";
          };
          dontUnpack = true;
          nativeBuildInputs = [ pkgs.unzip ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/lib" "$out/share/licenses/cudnn"
            unzip -j "$src" 'nvidia/cudnn/lib/*.so*' -d "$out/lib"
            unzip -j "$src" '*dist-info/licenses/*' -d "$out/share/licenses/cudnn"
            runHook postInstall
          '';
          meta.license = {
            fullName = "cuDNN Supplement to Software License Agreement for NVIDIA SDKs";
            url = "https://docs.nvidia.com/deeplearning/cudnn/backend/latest/reference/eula.html";
            free = false;
            redistributable = false;
          };
        };
        # The official ORT/OpenVINO wheel currently embeds OpenVINO 2025.4.1.
        # Keep a second evidence lane built from the same locked nixpkgs input
        # so current Intel GPU/NPU drivers can be tested against OpenVINO
        # 2026.3 without splicing runtime libraries at execution time.
        currentOpenvinoOrt = (pkgs.onnxruntime.override {
          pythonSupport = false;
          openvinoSupport = true;
          coremlSupport = false;
          cudaSupport = false;
          rocmSupport = false;
        }).overrideAttrs {
          # cfetch and the frozen 11-vector KAT are the acceptance tests for
          # this evidence package. Avoid compiling ORT's several-thousand-test
          # upstream suite as part of every hardware probe.
          doCheck = false;
        };
        nvidiaTensorRt = pkgs.stdenvNoCC.mkDerivation {
          pname = "cfetch-tensorrt-runtime";
          version = "10.16.1.11";
          src = pkgs.fetchurl {
            url = "https://pypi.nvidia.com/tensorrt-cu12-libs/tensorrt_cu12_libs-10.16.1.11-py3-none-manylinux_2_28_x86_64.whl";
            hash = "sha256-jkUDbv65ZNMjIxVERCpzYZIBE2zMhDklYCVMyPDVFuQ=";
          };
          dontUnpack = true;
          nativeBuildInputs = [ pkgs.unzip ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/lib" "$out/share/licenses/tensorrt"
            unzip -j "$src" 'tensorrt_libs/*.so*' -d "$out/lib"
            unzip -j "$src" '*dist-info/LICENSE*' -d "$out/share/licenses/tensorrt"
            runHook postInstall
          '';
          meta.license = {
            fullName = "TensorRT Supplement to Software License Agreement for NVIDIA SDKs";
            url = "https://docs.nvidia.com/deeplearning/tensorrt/latest/reference/sla.html";
            free = false;
            redistributable = false;
          };
        };
        cudaOrt = pkgs.symlinkJoin {
          name = "cfetch-cuda-onnxruntime-${ortVersion}";
          paths = [
            nvidiaOrt
            (pkgs.lib.getLib pkgs.cudaPackages.cuda_cudart)
            (pkgs.lib.getLib pkgs.cudaPackages.libcublas)
            (pkgs.lib.getLib pkgs.cudaPackages.libcurand)
          ];
          # Do not expose a TensorRT provider whose non-redistributable runtime
          # is absent from this package. The separate package below is explicit.
          postBuild = ''
            rm "$out/lib/libonnxruntime_providers_tensorrt.so"
          '';
        };
        tensorrtOrt = pkgs.symlinkJoin {
          name = "cfetch-tensorrt-onnxruntime-${ortVersion}";
          paths = [
            nvidiaOrt
            (pkgs.lib.getLib pkgs.cudaPackages.cuda_cudart)
            (pkgs.lib.getLib pkgs.cudaPackages.libcublas)
            (pkgs.lib.getLib pkgs.cudaPackages.libcurand)
            nvidiaCudnn
            nvidiaTensorRt
          ];
        };
        # Build the AMD provider from the same pinned nixpkgs input as cfetch.
        # Nixpkgs 7.2.3 otherwise disables RTTI while ORT 1.27.1's MIGraphX
        # stream-handle bridge uses dynamic_cast, so the unmodified derivation
        # fails to compile. This override is the smallest upstream-compatible
        # correction and retains the complete input-addressed ROCm closure.
        migraphxOrt = (pkgs.onnxruntime.override {
          pythonSupport = false;
          openvinoSupport = false;
          coremlSupport = false;
          cudaSupport = false;
          rocmSupport = true;
        }).overrideAttrs (old: {
          cmakeFlags = old.cmakeFlags ++ [
            (pkgs.lib.cmakeBool "onnxruntime_DISABLE_RTTI" false)
          ];
        });
        mkCfetch =
          {
            pname,
            localInference ? false,
            inferenceFeature ? "inference-ort",
            runtime ? certifiedOrt,
            runtimeDistribution ? "microsoft-github-release-v${ortVersion}",
            runtimeArchiveSha256 ? ortAsset.sha256,
            pluginDistribution ? null,
            pluginArchiveSha256 ? null,
            pluginLibrary ? null,
            hostDriverSearch ? false,
            runTests ? true,
          }:
          pkgs.rustPlatform.buildRustPackage {
          inherit pname;
          inherit version;
          doCheck = runTests;
          src = self;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # Cargo pins FastEmbed to the exact public session-controls commit;
            # Nix additionally requires a content hash before it will vendor a
            # Git dependency inside the sandbox.
            outputHashes = {
              "fastembed-6.0.0" =
                "sha256-IBBbYG1iClOKdSTML6dgrt9lt5kkHqVUllJ+gRYQdew=";
            };
          };
          buildFeatures = pkgs.lib.optionals localInference [ inferenceFeature ];
          CFETCH_ORT_DISTRIBUTION =
            if localInference then runtimeDistribution else null;
          CFETCH_ORT_ARCHIVE_SHA256 = if localInference then runtimeArchiveSha256 else null;
          CFETCH_EP_PLUGIN_DISTRIBUTION = pluginDistribution;
          CFETCH_EP_PLUGIN_ARCHIVE_SHA256 = pluginArchiveSha256;
          CFETCH_VARIANT =
            if localInference then
              null
            else if pkgs.stdenv.hostPlatform.isDarwin then
              if pkgs.stdenv.hostPlatform.isAarch64 then
                "mac-cfetch-remote-arm64"
              else
                "mac-cfetch-remote-x86_64"
            else if pkgs.stdenv.hostPlatform.isAarch64 then
              "linux-cfetch-remote-arm64"
            else
              "linux-cfetch-remote-x86_64";
          # One governance test creates and commits a temporary Git repository;
          # Nix build sandboxes do not otherwise put `git` on PATH.
          nativeBuildInputs = [ pkgs.git ]
            ++ pkgs.lib.optionals localInference [ pkgs.makeWrapper ];
          postInstall = ''
            install -Dm644 LICENSE.md "$out/share/licenses/cfetch/LICENSE.md"
            install -Dm644 THIRD-PARTY-LICENSES.txt \
              "$out/share/licenses/cfetch/THIRD-PARTY-LICENSES.txt"
          '' + pkgs.lib.optionalString localInference ''
            wrapProgram "$out/bin/cfetch" \
              --set-default ORT_DYLIB_PATH \
              "${runtime}/lib/${
                if pkgs.stdenv.hostPlatform.isDarwin then
                  "libonnxruntime.dylib"
                else
                  "libonnxruntime.so"
              }"${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux " \\
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath ([ runtime pkgs.stdenv.cc.cc.lib pkgs.zlib ] ++ pkgs.lib.optionals hostDriverSearch [ pkgs.stdenv.cc.libc ])}"}${pkgs.lib.optionalString hostDriverSearch " \\
              --suffix LD_LIBRARY_PATH : /run/opengl-driver/lib:/usr/lib:/opt/intel/oneapi/compiler/latest/lib"}${pkgs.lib.optionalString (pluginLibrary != null) " \\
              --set-default CFETCH_WEBGPU_LIBRARY ${pluginLibrary}"}
          '';

          meta = {
            description = "Cited, trust-tiered memory for AI coding agents over plain Markdown";
            homepage = "https://github.com/corbet-labs/cfetch";
            # FSL-1.1-ALv2 has no identifier in nixpkgs' license set — declare
            # it literally rather than mislabel it with a nearby SPDX id.
            # Source-available, converts to Apache-2.0 two years per release.
            license = {
              fullName = "Functional Source License, Version 1.1, ALv2 Future License";
              url = "https://fsl.software/FSL-1.1-ALv2.template.md";
              free = false;
              redistributable = true;
            };
            mainProgram = "cfetch";
          };
        };
        in
        rec {
        cfetch = mkCfetch { pname = "cfetch"; };
        cfetch-local-cpu = mkCfetch {
          pname = "cfetch-local-cpu";
          localInference = true;
        };
        default = cfetch;
      } // pkgs.lib.optionalAttrs (
        pkgs.stdenv.hostPlatform.isDarwin
        && pkgs.stdenv.hostPlatform.isAarch64
      ) {
        # Certification package, never a catalog claim. Microsoft's official
        # macOS ORT archive contains CoreML; physical compute-plan evidence and
        # exact bytes decide whether this host/provider may produce v1 vectors.
        cfetch-test-coreml = mkCfetch {
          pname = "cfetch-test-coreml";
          localInference = true;
          inferenceFeature = "inference-coreml";
        };
        cfetch-test-webgpu = mkCfetch {
          pname = "cfetch-test-webgpu";
          localInference = true;
          inferenceFeature = "inference-webgpu";
          # This is a hardware probe package. The blocking platform suite
          # already tests cfetch; rerunning unrelated iroh network tests here
          # made Metal evidence depend on a flaky test-server destructor.
          runTests = false;
          runtime = webgpuOrt;
          runtimeDistribution = "microsoft-github-release-v${ortVersion}+webgpu-plugin-${webgpuPluginVersion}";
          pluginDistribution = "nuget-Microsoft.ML.OnnxRuntime.EP.WebGpu-${webgpuPluginVersion}";
          pluginArchiveSha256 = webgpuPluginArchiveSha256;
          pluginLibrary = "${webgpuPlugin}/lib/${webgpuAsset.library}";
        };
      } // pkgs.lib.optionalAttrs (
        pkgs.stdenv.hostPlatform.isLinux
        && pkgs.stdenv.hostPlatform.isx86_64
      ) {
        # Intel's official wheel is a language-neutral ORT/OpenVINO runtime
        # distribution despite its wheel container. It includes the CPU, GPU
        # and NPU plugins. This remains an evidence package until a physical
        # device passes the KAT and placement review.
        cfetch-test-openvino = mkCfetch {
          pname = "cfetch-test-openvino";
          localInference = true;
          inferenceFeature = "inference-openvino";
          runtime = openvinoOrt;
          runtimeDistribution = "pypi-onnxruntime-openvino-${openvinoVersion}";
          runtimeArchiveSha256 = openvinoWheelSha256;
        };
        cfetch-test-openvino-current = mkCfetch {
          pname = "cfetch-test-openvino-current";
          localInference = true;
          inferenceFeature = "inference-openvino";
          runtime = currentOpenvinoOrt;
          hostDriverSearch = true;
          runtimeDistribution = "nixpkgs-${nixpkgs.rev}-onnxruntime-${currentOpenvinoOrt.version}-openvino-${pkgs.openvino.version}";
          # Nix-fixed SHA-256 of the ORT source at the flake-locked revision.
          runtimeArchiveSha256 = "8b6bbf2677db27fb2bb196370136f662c0415c48531a16adb2bdfef5e1d55773";
        };
        # AMD GPU evidence package. The distribution string pins every build
        # input through flake.lock; the recorded SHA-256 is the Nix-fixed ORT
        # source hash. The certification report separately hashes the exact
        # libonnxruntime loaded at execution time.
        cfetch-test-migraphx = mkCfetch {
          pname = "cfetch-test-migraphx";
          localInference = true;
          inferenceFeature = "inference-migraphx";
          runtime = migraphxOrt;
          runtimeDistribution = "nixpkgs-${nixpkgs.rev}-onnxruntime-1.27.1-migraphx-7.2.3";
          runtimeArchiveSha256 = "8b6bbf2677db27fb2bb196370136f662c0415c48531a16adb2bdfef5e1d55773";
        };
        # Both NVIDIA packages use Microsoft's exact CUDA 12 ORT release. CUDA
        # redistributables are Nix-pinned; TensorRT/cuDNN retain their upstream
        # non-redistributable licenses and are never copied into cfetch assets.
        cfetch-test-cuda = mkCfetch {
          pname = "cfetch-test-cuda";
          localInference = true;
          inferenceFeature = "inference-cuda";
          runtime = cudaOrt;
          runtimeDistribution = "microsoft-github-release-v${ortVersion}-cuda12+nixpkgs-cuda12.9";
          runtimeArchiveSha256 = "ea6bd2b65d7dfabbeb92c4af5dd8f12e5aed8601e544ad378d2f872275438b1a";
        };
        cfetch-test-tensorrt = mkCfetch {
          pname = "cfetch-test-tensorrt";
          localInference = true;
          inferenceFeature = "inference-tensorrt";
          runtime = tensorrtOrt;
          runtimeDistribution = "microsoft-github-release-v${ortVersion}-cuda12+nixpkgs-cuda12.9-tensorrt10.16";
          runtimeArchiveSha256 = "ea6bd2b65d7dfabbeb92c4af5dd8f12e5aed8601e544ad378d2f872275438b1a";
        };
        cfetch-test-webgpu = mkCfetch {
          pname = "cfetch-test-webgpu";
          localInference = true;
          inferenceFeature = "inference-webgpu";
          # The package's next step is the exact device KAT. Application tests
          # remain blocking checks, independently of this hardware probe.
          runTests = false;
          runtime = webgpuOrt;
          runtimeDistribution = "microsoft-github-release-v${ortVersion}+webgpu-plugin-${webgpuPluginVersion}";
          pluginDistribution = "nuget-Microsoft.ML.OnnxRuntime.EP.WebGpu-${webgpuPluginVersion}";
          pluginArchiveSha256 = webgpuPluginArchiveSha256;
          pluginLibrary = "${webgpuPlugin}/lib/${webgpuAsset.library}";
        };
      });

      checks = forAllSystems (pkgs: {
        # buildRustPackage already runs `cargo test` in checkPhase.
        cfetch = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch;
        cfetch-local-cpu = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch-local-cpu;
      } // pkgs.lib.optionalAttrs (
        pkgs.stdenv.hostPlatform.isDarwin
        && pkgs.stdenv.hostPlatform.isAarch64
      ) {
        cfetch-test-coreml = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch-test-coreml;
      } // pkgs.lib.optionalAttrs (
        pkgs.stdenv.hostPlatform.isLinux
        && pkgs.stdenv.hostPlatform.isx86_64
      ) {
        cfetch-test-openvino = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch-test-openvino;
      });
    };
}
