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
            config.allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "cfetch";
          })
        );
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs: rec {
        cfetch = pkgs.rustPlatform.buildRustPackage {
          pname = "cfetch";
          inherit version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "A second brain for coding agents: privilege-ring memory, hook injection, retrieval, and a code index in one binary";
            homepage = "https://github.com/julian-corbet/cfetch";
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
        default = cfetch;
      });

      checks = forAllSystems (pkgs: {
        # buildRustPackage already runs `cargo test` in checkPhase.
        cfetch = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch;
      });
    };
}
