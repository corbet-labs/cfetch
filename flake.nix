# Nix packaging for cfetch. Native accelerator runtimes return here only after
# they satisfy the NPU-first mixed-backend admission contract.
{
  description = "cfetch — a second brain for coding agents: privilege-ring memory, hook injection, retrieval, and a code index in one binary";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (import nixpkgs {
            inherit system;
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
          CFETCH_VARIANT =
            if pkgs.stdenv.hostPlatform.isDarwin then
              if pkgs.stdenv.hostPlatform.isAarch64 then
                "mac-cfetch-remote-arm64"
              else
                "mac-cfetch-remote-x86_64"
            else if pkgs.stdenv.hostPlatform.isAarch64 then
              "linux-cfetch-remote-arm64"
            else
              "linux-cfetch-remote-x86_64";
          postInstall = ''
            install -Dm644 LICENSE.md "$out/share/licenses/cfetch/LICENSE.md"
            install -Dm644 THIRD-PARTY-LICENSES.txt +              "$out/share/licenses/cfetch/THIRD-PARTY-LICENSES.txt"
          '';

          meta = {
            description = "Cited, trust-tiered memory for AI coding agents over plain Markdown";
            homepage = "https://github.com/corbet-labs/cfetch";
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
        cfetch = self.packages.${pkgs.stdenv.hostPlatform.system}.cfetch;
      });
    };
}
