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
              builtins.elem (nixpkgs.lib.getName pkg) [ "cfetch" "cfetch-local-cpu" ];
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
        mkCfetch =
          {
            pname,
            localInference ? false,
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
                "sha256-rW6wq3wASoKBXvBHnia0cECYPtD8po1EPXV1gx2rg7E=";
            };
          };
          buildFeatures = pkgs.lib.optionals localInference [ "inference-ort" ];
          CFETCH_ORT_DISTRIBUTION =
            if localInference then "microsoft-github-release-v${ortVersion}" else null;
          CFETCH_ORT_ARCHIVE_SHA256 = if localInference then ortAsset.sha256 else null;
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
              "${certifiedOrt}/lib/${
                if pkgs.stdenv.hostPlatform.isDarwin then
                  "libonnxruntime.dylib"
                else
                  "libonnxruntime.so"
              }"${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux " \\
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ]}"}
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
      });

      checks = forAllSystems (pkgs: {
        # buildRustPackage already runs `cargo test` in checkPhase.
        cfetch = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch;
        cfetch-local-cpu = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch-local-cpu;
      });
    };
}
