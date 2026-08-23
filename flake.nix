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
                "sha256-uDLesOjegkXWUzjOlGFUdoAO2m/p85PGSl2zuC89eHM=";
            };
          };
          buildFeatures = pkgs.lib.optionals localInference [ "inference-ort" ];
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
              "${pkgs.onnxruntime}/lib/${
                if pkgs.stdenv.hostPlatform.isDarwin then
                  "libonnxruntime.dylib"
                else
                  "libonnxruntime.so"
              }"
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
