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
              builtins.elem (nixpkgs.lib.getName pkg) [
                "cfetch"
                "cfetch-local-cpu"
                "cfetch-test-coreml"
                "cfetch-test-openvino"
              ];
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
        mkCfetch =
          {
            pname,
            localInference ? false,
            inferenceFeature ? "inference-ort",
            runtime ? certifiedOrt,
            runtimeDistribution ? "microsoft-github-release-v${ortVersion}",
            runtimeArchiveSha256 ? ortAsset.sha256,
          }:
          pkgs.rustPlatform.buildRustPackage {
          inherit pname;
          inherit version;
          src = self;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # Cargo pins FastEmbed to the exact public session-controls commit;
            # Nix additionally requires a content hash before it will vendor a
            # Git dependency inside the sandbox.
            outputHashes = {
              "fastembed-6.0.0" =
                "sha256-wM3644tWbej0NJX+k7utXNtAMlMe6vFpe0iYPu0fczE=";
            };
          };
          buildFeatures = pkgs.lib.optionals localInference [ inferenceFeature ];
          CFETCH_ORT_DISTRIBUTION =
            if localInference then runtimeDistribution else null;
          CFETCH_ORT_ARCHIVE_SHA256 = if localInference then runtimeArchiveSha256 else null;
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
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [ runtime pkgs.stdenv.cc.cc.lib pkgs.zlib ]}"}
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
